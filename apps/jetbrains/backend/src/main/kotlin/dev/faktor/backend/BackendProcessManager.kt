// The Faktor backend process manager: it launches the faktor-cli binary,
// parses the startup line from stdout, and carries the frontend-generated
// password/port for the native client (NativeClient / NativeEventStream).
//
// Rules observed:
//  - stdout is drained on a dedicated thread so a full pipe never deadlocks
//    the manager; the ring buffer is bounded (1024 lines).
//  - stop() is SIGTERM first, destroyForcibly only after a grace period;
//    the drain thread stops with the process (no orphans, no leak).
//  - a crashed/missing daemon fails loudly: start() throws with the exit
//    code and a bounded stdout tail.
package dev.faktor.backend

import dev.faktor.shared.StartupLine
import java.io.BufferedReader
import java.io.IOException
import java.io.InputStreamReader
import java.nio.file.Files
import java.nio.file.Path
import java.security.SecureRandom
import java.util.ArrayDeque
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/** A daemon-side failure. [status] carries the HTTP status when one applies. */
class BackendException(
    message: String,
    val status: Int? = null,
    cause: Throwable? = null
) : Exception(message, cause)

private const val MAX_BUFFERED_LINES = 1024

/** A running daemon plus everything needed to talk to and stop it. */
class BackendConnection(
    val port: Int,
    val password: String,
    val process: Process,
    internal val sink: StdoutSink
) {
    internal fun recentLines(max: Int): List<String> = sink.recentLines(max)
    val baseUrl: String = "http://127.0.0.1:$port"

    fun isAlive(): Boolean = process.isAlive

    fun pid(): Long = process.pid()
}

/**
 * Manages the Faktor daemon lifecycle. The binary must be the faktor-cli
 * executable; it is launched as `serve --port 0 --data-dir <dir>` with the
 * generated password in `FAKTOR_SERVER_PASSWORD`.
 */
class BackendProcessManager(private val binaryPath: Path, private val dataDir: Path) {

    companion object {
        private const val STARTUP_TIMEOUT_MS = 20_000L
        private const val STOP_GRACE_MS = 3_000L
        private const val PASSWORD_HEX_BYTES = 32
    }

    private val rng = SecureRandom()

    /** Starts the daemon and waits (bounded) for the frozen startup line. */
    fun start(): BackendConnection {
        if (!Files.isRegularFile(binaryPath) || !Files.isExecutable(binaryPath)) {
            throw BackendException("faktor-cli binary not found or not executable: $binaryPath")
        }
        val password = generatePassword()
        val pb = ProcessBuilder(
            binaryPath.toAbsolutePath().toString(),
            "serve", "--port", "0",
            "--data-dir", dataDir.toAbsolutePath().toString()
        )
        pb.environment()["FAKTOR_SERVER_PASSWORD"] = password
        // stdout is the startup-line contract and is drained by StdoutSink;
        // daemon logs ride stderr to the IDE console.
        pb.redirectError(ProcessBuilder.Redirect.INHERIT)
        val process = try {
            pb.start()
        } catch (e: IOException) {
            throw BackendException("failed to launch the faktor-cli binary: ${e.message}", cause = e)
        }
        val sink = StdoutSink(process)
        val port = sink.awaitStartupLine(STARTUP_TIMEOUT_MS)
        if (port == null) {
            val exit = try {
                process.exitValue()
            } catch (e: IllegalThreadStateException) {
                null
            }
            val tail = sink.recentLines(20)
            process.destroyForcibly()
            sink.stop()
            val reason = if (exit != null) {
                "daemon exited with code $exit before the startup line"
            } else {
                "startup line not seen within ${STARTUP_TIMEOUT_MS}ms"
            }
            throw BackendException(
                "$reason; stdout tail: ${tail.joinToString(" | ")}"
            )
        }
        return BackendConnection(port, password, process, sink)
    }

    /** SIGTERM, then destroyForcibly after [STOP_GRACE_MS]; stops the drainer. */
    fun stop(connection: BackendConnection) {
        val process = connection.process
        process.destroy()
        if (!process.waitFor(STOP_GRACE_MS, TimeUnit.MILLISECONDS)) {
            process.destroyForcibly()
            process.waitFor(1, TimeUnit.SECONDS)
        }
        connection.sink.stop()
    }

    private fun generatePassword(): String {
        val bytes = ByteArray(PASSWORD_HEX_BYTES)
        rng.nextBytes(bytes)
        val sb = StringBuilder(bytes.size * 2)
        for (b in bytes) sb.append(String.format("%02x", b))
        return sb.toString()
    }
}

/**
 * Drains the daemon stdout on a dedicated daemon thread into a bounded ring
 * buffer, and latches the startup line when it appears. EOF or process exit
 * without the startup line releases the latch so start() fails fast.
 */
class StdoutSink(process: Process) {
    private val lock = Object()
    private val ring = ArrayDeque<String>()
    private val startup = CountDownLatch(1)

    @Volatile
    private var startupPort: Int? = null

    @Volatile
    private var stopped = false

    private val thread: Thread = Thread({ drain(process) }, "faktor-stdout-drain")

    init {
        thread.isDaemon = true
        // Wake the latch if the process exits without printing the line.
        process.onExit().whenComplete { _, _ ->
            if (startupPort == null) startup.countDown()
        }
        thread.start()
    }

    /** Waits (bounded) for the startup line; null on timeout or early exit. */
    fun awaitStartupLine(timeoutMs: Long): Int? {
        val deadline = System.currentTimeMillis() + timeoutMs
        if (!startup.await(timeoutMs, TimeUnit.MILLISECONDS)) return null
        // The latch may have been tripped by onExit a micro-instant before
        // the drainer parsed the line; give it a brief chance.
        while (startupPort == null && System.currentTimeMillis() < deadline) {
            Thread.sleep(10)
        }
        return startupPort
    }

    fun recentLines(max: Int): List<String> = synchronized(lock) {
        ring.toList().takeLast(max)
    }

    fun stop() {
        stopped = true
        thread.interrupt()
        try {
            thread.join(1000)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
        }
    }

    private fun drain(process: Process) {
        val reader = BufferedReader(InputStreamReader(process.inputStream, Charsets.UTF_8))
        try {
            while (!stopped) {
                val line = reader.readLine() ?: break
                val parsed = StartupLine.parse(line)
                if (parsed != null && startupPort == null) {
                    startupPort = parsed.port
                    startup.countDown()
                }
                synchronized(lock) {
                    ring.addLast(line)
                    while (ring.size > MAX_BUFFERED_LINES) {
                        ring.removeFirst()
                    }
                }
            }
        } catch (e: IOException) {
            // Stream closed under us (e.g. destroyForcibly during stop()).
        }
        if (startupPort == null) startup.countDown()
    }
}
