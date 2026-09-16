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
 * Nothing else is ever printed on stdout; there is no JSON handshake.
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
