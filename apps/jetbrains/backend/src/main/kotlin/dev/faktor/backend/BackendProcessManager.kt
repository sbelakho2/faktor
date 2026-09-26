// The Faktor backend process manager: it launches the faktor-cli binary,
// parses the startup line from stdout, and carries the frontend-generated
// password/port for the native client (NativeClient / NativeEventStream).
//
// Rules observed:
//  - stdout is drained on a dedicated thread so a full pipe never deadlocks
//    the manager; the ring buffer is bounded (1024 lines).
//  - the connection tracks the RELEASE daemon process, never a launcher pid:
//    the release-pid handshake line when the launcher announces it, else the
//    listening-port probe (Windows), else the live spawned child (unix
//    `execve` replaces the launcher, so the child IS the release). A
//    bootstrap launcher that exits immediately after readiness is never
//    mistaken for daemon death.
//  - the handshake pid is TRUST-BOUND: its digest is compared against the
//    digest the daemon reports through /native/health, and a probe pid that
//    disagrees refuses the announcement. A refused announcement is never
//    adopted or signalled (typed ReleaseTrustFailure); stop() re-verifies
//    health identity (and the announcement digest) before signalling any
//    non-child pid, so a recycled pid is refused typed.
//  - liveness always falls back to the /native/health probe: the launcher
//    wrapper's exit is never daemon death, and a healthy daemon is never
//    reported dead just because no release pid could be resolved.
//  - stop() terminates the RELEASE gracefully (SIGTERM / taskkill), force
//    after a grace period, and only then cleans up a resident launcher;
//    the drain thread stops with the process (no orphans, no leak).
//  - a crashed/missing daemon fails loudly: start() throws with the exit
//    code and a bounded stdout tail.
package dev.faktor.backend

import dev.faktor.shared.ReleaseDigest
import dev.faktor.shared.ReleasePidHandshake
import dev.faktor.shared.ReleasePidLine
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

/**
 * A typed refusal to trust/adopt (or signal) a release-pid source. [code] is
 * stable for assertions; [message] names the exact check that failed.
 */
data class ReleaseTrustFailure(val code: String, val message: String) {
    override fun toString(): String = "$code: $message"
}

private const val MAX_BUFFERED_LINES = 1024
private const val PID_PROBE_TIMEOUT_MS = 5_000L
private const val STOP_POLL_MS = 50L
private const val HEALTH_PROBE_TIMEOUT_MS = 2_000L
private const val ANNOUNCEMENT_VERIFY_MS = 3_000L

/**
 * A running daemon plus everything needed to talk to and stop it.
 *
 * [releasePid] is the RELEASE (serving daemon) pid once resolved; `null` means
 * no pid could be resolved and liveness is health-only. [announced] is the
 * launcher handshake identity this pid came from (when it did), and
 * [releaseTrustError] is non-null when an announcement was refused or could
 * not be verified: such a pid is never signalled by [isAlive]/stop paths
 * without a live health-digest match.
 */
class BackendConnection(
    val port: Int,
    val password: String,
    val process: Process,
    internal val sink: StdoutSink,
    private val releasePid: Long? = null,
    private val announced: ReleasePidHandshake? = null,
    val releaseTrustError: ReleaseTrustFailure? = null
) {
    internal fun recentLines(max: Int): List<String> = sink.recentLines(max)
    val baseUrl: String = "http://127.0.0.1:$port"

    @Volatile
    var stopRefusal: ReleaseTrustFailure? = null
        private set

    internal fun recordStopRefusal(failure: ReleaseTrustFailure) {
        stopRefusal = failure
    }

    internal fun resolvedReleasePid(): Long? = releasePid

    internal fun announcedIdentity(): ReleasePidHandshake? = announced

    /**
     * True while the RELEASE daemon answers /native/health. A resolved
     * release pid that is dead short-circuits false; every other case
     * (launcher-only child, unresolved pid, resident wrapper) is decided by
     * the health probe — the launcher's exit is never daemon death.
     */
    fun isAlive(): Boolean {
        val pid = releasePid
        if (pid != null && pid > 0 && pid != process.pid() && !pidAlive(pid)) {
            return false
        }
        return healthAlive()
    }

    /** The RELEASE pid when resolved; never a stale launcher pid. */
    fun pid(): Long = releasePid ?: process.pid()

    private fun healthAlive(): Boolean = try {
        NativeClient(baseUrl, password, timeoutMs = HEALTH_PROBE_TIMEOUT_MS).health().ok
    } catch (e: Exception) {
        false
    }

    /**
     * The stop-time identity check for a non-child target: the process must
     * be alive AND the daemon on [port] must still answer /native/health with
     * our connection password (a recycled pid cannot). When the pid came from
     * the handshake, the health version's release digest must still equal the
     * announced one. A fresh port probe naming a DIFFERENT pid also refuses
     * the cached pid. Returns the typed refusal, or null when signalling is
     * safe.
     */
    internal fun verifyStopIdentity(probe: (Int) -> Long?): ReleaseTrustFailure? {
        val pid = releasePid ?: return ReleaseTrustFailure(
            "stop_identity_unverified",
            "refusing to signal: no verified release pid is resolved for port $port"
        )
        val probed = try {
            probe(port)
        } catch (e: Exception) {
            null
        }
        if (probed != null && probed != pid) {
            return ReleaseTrustFailure(
                "release_pid_probe_mismatch",
                "refusing to signal pid $pid: the port $port probe now names pid $probed"
            )
        }
        val version = try {
            val health = NativeClient(baseUrl, password, timeoutMs = HEALTH_PROBE_TIMEOUT_MS).health()
            if (health.ok) health.version else null
        } catch (e: Exception) {
            null
        }
        if (version == null) {
            return ReleaseTrustFailure(
                "stop_identity_unverified",
                "refusing to signal pid $pid: the daemon at port $port did not answer /native/health"
            )
        }
        val announcedHere = announced
        if (announcedHere != null && announcedHere.pid == pid) {
            val digest = ReleaseDigest.of(version)
            if (digest == null) {
                return ReleaseTrustFailure(
                    "release_digest_absent",
                    "refusing to signal announced pid $pid: the daemon reports no release digest to confirm it"
                )
            }
            if (digest != announcedHere.digest) {
                return ReleaseTrustFailure(
                    "release_digest_mismatch",
                    "refusing to signal announced pid $pid: the daemon reports digest $digest, " +
                        "not the announced ${announcedHere.digest}"
                )
            }
        }
        return null
    }
}

/**
 * Manages the Faktor daemon lifecycle. The binary must be the faktor-cli
 * executable; it is launched as `serve --port 0 --data-dir <dir>` with the
 * generated password in `FAKTOR_SERVER_PASSWORD`.
 *
 * [releasePidResolver] resolves the release process that owns the daemon's
 * listening port (default: the Windows `netstat` probe; unix needs none
 * because `execve` makes the spawned child the release). It is injectable
 * for embedders/tests.
 */
class BackendProcessManager(
    private val binaryPath: Path,
    private val dataDir: Path,
    private val releasePidResolver: (Int) -> Long? = { port -> probeListeningPid(port) }
) {

    companion object {
        // Must exceed the bootstrap launcher's ~30 s launch window: the
        // supervisor must not declare failure while the updater launcher may
        // still be bringing the release to readiness (and may still be
        // adoptable).
        private const val STARTUP_TIMEOUT_MS = 45_000L
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
        // The RELEASE pid, in order: the launcher's announced handshake pid
        // (only after the trust binding below), the injected/platform port
        // probe, the live spawned child (unix execve, or a legacy resident
        // launcher still wrapping its release).
        val announcement = sink.releaseAnnouncement()
        val probed = try {
            releasePidResolver(port)
        } catch (e: Exception) {
            null
        }
        var refused: ReleaseTrustFailure? = null
        var trusted: ReleasePidHandshake? = null
        if (announcement != null) {
            if (probed != null && probed != announcement.pid) {
                refused = ReleaseTrustFailure(
                    "release_pid_probe_mismatch",
                    "the release-pid handshake announced pid ${announcement.pid} but the port $port " +
                        "probe names pid $probed; the announced pid was not adopted"
                )
            } else {
                when (val verdict = verifyAnnouncementDigest(announcement, port, password)) {
                    is AnnouncementVerdict.Trusted -> trusted = announcement
                    // Still bringing the HTTP surface up: adopt tentatively.
                    // stop() demands a live digest match before signalling.
                    is AnnouncementVerdict.Unreachable -> trusted = announcement
                    is AnnouncementVerdict.Refused -> refused = verdict.failure
                }
            }
        }
        val releasePid = trusted?.pid
            ?: probed
            ?: process.pid().takeIf { process.isAlive }
        return BackendConnection(port, password, process, sink, releasePid, trusted, refused)
    }

    /**
     * Terminates the RELEASE daemon gracefully (SIGTERM / taskkill), force
     * after [STOP_GRACE_MS]; the resident launcher is cleaned up afterwards
     * and the drainer stops with the process. A non-child pid is signalled
     * only after the health identity (and, for an announced pid, the release
     * digest) still verifies; otherwise the refusal is recorded typed on the
     * connection and nothing is signalled.
     */
    fun stop(connection: BackendConnection) {
        val releasePid = connection.resolvedReleasePid()
        val launcherPid = connection.process.pid()
        var signalled = false
        if (releasePid != null && releasePid != launcherPid && pidAlive(releasePid)) {
            val refusal = connection.verifyStopIdentity(releasePidResolver)
            if (refusal != null) {
                connection.recordStopRefusal(refusal)
            } else {
                signalled = true
                terminatePid(releasePid, force = false)
                val deadline = System.currentTimeMillis() + STOP_GRACE_MS
                while (pidAlive(releasePid) && System.currentTimeMillis() < deadline) {
                    Thread.sleep(STOP_POLL_MS)
                }
                if (pidAlive(releasePid)) {
                    terminatePid(releasePid, force = true)
                }
            }
        }
        val process = connection.process
        if (process.isAlive) {
            process.destroy()
            if (!process.waitFor(STOP_GRACE_MS, TimeUnit.MILLISECONDS)) {
                process.destroyForcibly()
                process.waitFor(1, TimeUnit.SECONDS)
            }
        }
        if (!signalled && connection.stopRefusal == null && connection.releaseTrustError != null) {
            // Nothing was signalled and the only candidate was an already
            // refused announcement: surface that typed refusal instead of a
            // silent no-op.
            connection.recordStopRefusal(connection.releaseTrustError)
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

/** The announcement-digest verification verdict. */
private sealed class AnnouncementVerdict {
    /** The daemon's health digest equals the announced one. */
    object Trusted : AnnouncementVerdict()

    /** The HTTP surface did not answer within the bounded window. */
    object Unreachable : AnnouncementVerdict()

    /** The daemon answered but the announcement cannot be confirmed. */
    class Refused(val failure: ReleaseTrustFailure) : AnnouncementVerdict()
}

/**
 * Confirm the handshake announcement against the daemon's own health digest,
 * with a bounded retry while the HTTP surface comes up. A reachable daemon
 * with an absent or different digest is a typed refusal.
 */
private fun verifyAnnouncementDigest(
    announcement: ReleasePidHandshake,
    port: Int,
    password: String
): AnnouncementVerdict {
    val deadline = System.currentTimeMillis() + ANNOUNCEMENT_VERIFY_MS
    while (true) {
        val version = try {
            val health = NativeClient(
                "http://127.0.0.1:$port",
                password,
                timeoutMs = HEALTH_PROBE_TIMEOUT_MS
            ).health()
            if (health.ok) health.version else null
        } catch (e: Exception) {
            null
        }
        if (version != null) {
            val digest = ReleaseDigest.of(version)
            return when {
                digest == null -> AnnouncementVerdict.Refused(
                    ReleaseTrustFailure(
                        "release_digest_absent",
                        "the daemon at port $port reports no release digest while the launcher announced " +
                            "digest ${announcement.digest} for pid ${announcement.pid}; the announced pid was not adopted"
                    )
                )
                digest != announcement.digest -> AnnouncementVerdict.Refused(
                    ReleaseTrustFailure(
                        "release_digest_mismatch",
                        "the daemon at port $port reports release digest $digest, not the launcher-announced " +
                            "${announcement.digest} for pid ${announcement.pid}; the announced pid was not adopted"
                    )
                )
                else -> AnnouncementVerdict.Trusted
            }
        }
        if (System.currentTimeMillis() >= deadline) return AnnouncementVerdict.Unreachable
        Thread.sleep(100L)
    }
}

// --------------------------------------------------------- process probes

private fun isWindows(): Boolean =
    System.getProperty("os.name", "").toLowerCase().contains("win")

/** True while `pid` names a live process (a zombie still owns its pid). */
internal fun pidAlive(pid: Long): Boolean {
    if (pid <= 0) return false
    return if (isWindows()) {
        val out = runCommand(
            listOf("tasklist", "/FI", "PID eq $pid", "/FO", "CSV", "/NH"),
            PID_PROBE_TIMEOUT_MS
        )
        out != null && out.contains(",\"$pid\",")
    } else {
        exitCode(listOf("kill", "-0", pid.toString())) == 0
    }
}

/** SIGTERM / taskkill, or the forced form on escalation. */
internal fun terminatePid(pid: Long, force: Boolean) {
    if (pid <= 0) return
    if (isWindows()) {
        val command = mutableListOf("taskkill", "/PID", pid.toString())
        if (force) command.add("/F")
        runCommand(command, PID_PROBE_TIMEOUT_MS)
    } else {
        runCommand(listOf("kill", if (force) "-KILL" else "-TERM", pid.toString()), PID_PROBE_TIMEOUT_MS)
    }
}

/** The owning pid of `127.0.0.1:<port>` (or `[::1]:<port>`) in LISTENING. */
private fun probeListeningPid(port: Int): Long? {
    if (!isWindows()) return null
    val output = runCommand(listOf("netstat", "-ano", "-p", "tcp"), PID_PROBE_TIMEOUT_MS) ?: return null
    for (line in output.lines()) {
        val columns = line.trim().split(Regex("\\s+"))
        if (columns.size >= 5 &&
            columns[0].equals("TCP", ignoreCase = true) &&
            columns[1].endsWith(":$port") &&
            columns[3].equals("LISTENING", ignoreCase = true)
        ) {
            val pid = columns[4].toLongOrNull()
            if (pid != null && pid > 0) return pid
        }
    }
    return null
}

/** Bounded command capture; null on launch failure, timeout or nonzero exit. */
private fun runCommand(command: List<String>, timeoutMs: Long): String? {
    return try {
        val process = ProcessBuilder(command).redirectErrorStream(true).start()
        if (!process.waitFor(timeoutMs, TimeUnit.MILLISECONDS)) {
            process.destroyForcibly()
            return null
        }
        val bytes = process.inputStream.readBytes()
        if (process.exitValue() != 0) null else String(bytes, Charsets.UTF_8)
    } catch (e: Exception) {
        null
    }
}

private fun exitCode(command: List<String>): Int? {
    return try {
        val process = ProcessBuilder(command).redirectErrorStream(true).start()
        if (!process.waitFor(PID_PROBE_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
            process.destroyForcibly()
            null
        } else {
            process.exitValue()
        }
    } catch (e: Exception) {
        null
    }
}

/**
 * Drains the daemon stdout on a dedicated daemon thread into a bounded ring
 * buffer, and latches the startup line when it appears. EOF or process exit
 * without the startup line releases the latch so start() fails fast. The
 * launcher's release-pid handshake line is latched too (it precedes the
 * startup line), so the supervisor tracks the release, not the launcher.
 */
class StdoutSink(process: Process) {
    private val lock = Object()
    private val ring = ArrayDeque<String>()
    private val startup = CountDownLatch(1)

    @Volatile
    private var startupPort: Int? = null

    @Volatile
    private var announcedHandshake: ReleasePidHandshake? = null

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

    /** The launcher-announced RELEASE identity (pid + digest), or null. */
    fun releaseAnnouncement(): ReleasePidHandshake? = announcedHandshake

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
                if (announcedHandshake == null) {
                    val announced = ReleasePidLine.parse(line)
                    if (announced != null) {
                        announcedHandshake = announced
                    }
                }
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
