// The daemon's process/stdio contract as plain Kotlin types.
//
// Zero external dependencies on purpose: no kotlinx.serialization, no okhttp,
// no org.json. The daemon's HTTP surface is Faktor Native Protocol v1 and is
// parsed by NativeProtocol.kt; this file keeps only the startup line the
// process manager reads from stdout and the Faktor-native bearer auth form.
package dev.faktor.shared

import java.util.regex.Pattern

/** Thrown when a daemon stdout line violates the startup-line contract. */
class ProtocolException(message: String) : Exception(message)

/**
 * The daemon stdout line: `faktor server listening on http://127.0.0.1:<port>`.
 * The daemon itself prints nothing else on stdout; a non-unix bootstrap
 * launcher prepends only the frozen release-started line ([ReleasePidLine])
 * on the same stream. There is no JSON handshake.
 */
data class StartupLine(val port: Int) {
    override fun toString(): String = "faktor server listening on http://127.0.0.1:$port"

    companion object {
        private val PATTERN: Pattern =
            Pattern.compile("^faktor server listening on http://127\\.0\\.0\\.1:(\\d+)$")

        /** Parses one exact startup line; returns null for any other content. */
        fun parse(line: String): StartupLine? {
            val m = PATTERN.matcher(line)
            if (!m.matches()) return null
            return StartupLine(m.group(1).toInt())
        }
    }
}

/**
 * One parsed release-pid handshake: the RELEASE pid AND the digest the
 * launcher verified. The digest is the trust anchor: the supervisor must
 * compare it against the digest the daemon itself reports through
 * `/native/health` before it adopts the pid for tracking/stopping.
 */
data class ReleasePidHandshake(val pid: Long, val digest: String)

/**
 * The bootstrap launcher's release-pid handshake line, printed on the
 * launcher's stdout immediately after the release is spawned and before the
 * startup line is forwarded (frozen in `crates/updater/src/release.rs`,
 * `launch()`, right after `command.spawn()`):
 *
 *   faktor release started pid=<pid> digest=<64 lowercase hex>
 *
 * The pid is the RELEASE process the supervisor must track: the launcher is
 * replaced by the release on unix (`execve`) and exits as soon as readiness
 * and the digest are proven on non-unix (bounded by the 30 s launch window),
 * so the launcher's own pid goes stale the moment the daemon is healthy.
 * When the line does not appear, the supervisor falls back to the
 * listening-port probe and the live spawned child.
 */
object ReleasePidLine {
    private val PATTERN: Pattern =
        Pattern.compile("^faktor release started pid=([1-9]\\d*) digest=([0-9a-f]{64})$")

    /** Parses one exact handshake line; returns null for any other content. */
    fun parse(line: String): ReleasePidHandshake? {
        val m = PATTERN.matcher(line)
        if (!m.matches()) return null
        val pid = m.group(1).toLongOrNull() ?: return null
        return ReleasePidHandshake(pid, m.group(2))
    }
}

/**
 * The daemon's health `version` carries the bootstrap-verified release digest
 * as `+release.<id>.<64 lowercase hex>` (the same shape the VS Code
 * supervisor parses). `of` returns the digest, or null when the version has
 * none — an honest absence, never a guess.
 */
object ReleaseDigest {
    private val PATTERN: Pattern =
        Pattern.compile("\\+release\\.([A-Za-z0-9._+-]+)\\.([0-9a-f]{64})$")

    fun of(version: String): String? {
        val m = PATTERN.matcher(version)
        return if (m.find()) m.group(2) else null
    }
}

/**
 * Faktor-native auth for every daemon request:
 * `Authorization: Bearer <FAKTOR_SERVER_PASSWORD>`. The daemon compares the
 * claim constant-time against its own password; revoked pre-cutover clients
 * migrate from the retired Basic compatibility form to this one, and there is
 * no downgrade path.
 */
class BearerAuth(val password: String) {
    val headerValue: String
        get() = "Bearer $password"

    companion object {
        const val HEADER_NAME: String = "Authorization"
    }
}

/**
 * ASCII-only lowercase that compiles across the whole supported kotlinc
 * range: 1.3 has no `String.lowercase()`, and kotlinc >= 1.5 rejects the
 * deprecated `String.toLowerCase()` under the split-mode smoke's warning
 * policy. Only ASCII case is folded, which is all these call sites need
 * (OS names, MIME extensions, endpoint schemes/hosts).
 */
fun asciiLowerCase(text: String): String {
    val out = StringBuilder(text.length)
    for (ch in text) out.append(Character.toLowerCase(ch))
    return out.toString()
}
