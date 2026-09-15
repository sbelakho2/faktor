// The daemon's process/stdio contract as plain Kotlin types.
//
// Zero external dependencies on purpose: no kotlinx.serialization, no okhttp,
// no org.json. The daemon's HTTP surface is Faktor Native Protocol v1 and is
// parsed by NativeProtocol.kt; this file keeps only the startup line the
// process manager reads from stdout and the Basic auth header form.
package dev.faktor.shared

import java.util.Base64
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
 * Basic auth for every daemon request:
 * `Authorization: Basic base64("kilo:" + FAKTOR_SERVER_PASSWORD)`. The daemon
 * splits the decoded payload at the FIRST colon; username must be exactly
 * `kilo`; the password is compared constant-time.
 */
class BasicAuth(val password: String) {
    val headerValue: String
        get() = "Basic " + Base64.getEncoder()
            .encodeToString((BasicAuth.USERNAME + ":" + password).toByteArray(Charsets.UTF_8))

    companion object {
        const val HEADER_NAME: String = "Authorization"
        const val USERNAME: String = "kilo"
    }
}

