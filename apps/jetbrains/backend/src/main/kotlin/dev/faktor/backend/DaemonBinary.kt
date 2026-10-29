// Application/machine-scoped Faktor daemon binary resolution and trust.
//
// The daemon executable is resolved from MACHINE-scoped inputs only:
//   1. the application-level Faktor binary path (the plugin's own
//      application setting, written by "Locate daemon…"),
//   2. FAKTOR_BIN,
//   3. the signed install layout at ~/.faktor/install (<root>/launcher),
//      the app-level location the faktor-cli installer/updater manages.
// A project/workspace is NEVER consulted: no project target/debug|release
// lookup exists, and a repository-supplied path can therefore never select
// the daemon. With no machine-scoped input, resolution fails with an
// actionable missing-daemon refusal naming the installer location and the
// "Locate daemon…" action.
//
// Where a checksum record exists beside the resolved executable
// (`<binary>.sha256`), the executable MUST match it before the daemon may
// start; a missing record is an explicit unverified state that the host UI
// must warn about and require the operator to confirm.
package dev.faktor.backend

import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths
import java.security.MessageDigest

/**
 * Pure, IntelliJ-free resolution + trust rules for the daemon executable.
 * Every input is machine-scoped; this object has no project/workspace input
 * by construction.
 */
object DaemonBinary {

    /** The environment override, checked only when no application setting exists. */
    const val ENV_BIN = "FAKTOR_BIN"

    /** The app-level install layout root relative to the user home. */
    val INSTALL_RELATIVE: Path = Paths.get(".faktor", "install")

    /** The trusted install root (the app-level signed layout). */
    fun trustedInstallRoot(home: Path): Path = home.resolve(INSTALL_RELATIVE).normalize()

    /**
     * True when [executable] resolves INSIDE the trusted install root — the
     * only location an automatic (unattended) start may execute from. An
     * explicit operator start may still run a confirmed binary elsewhere.
     */
    fun isInsideTrustedInstall(executable: Path, home: Path): Boolean =
        executable.toAbsolutePath().normalize().startsWith(trustedInstallRoot(home))

    /** The stable bootstrap launcher inside the install layout. */
    const val LAUNCHER_NAME = "launcher"

    /** The optional checksum-record suffix: `<binary>.sha256`. */
    const val CHECKSUM_SUFFIX = ".sha256"

    private val SHA256_HEX = Regex("[0-9a-f]{64}")

    /** A resolved executable plus the machine-scoped source that produced it. */
    data class Resolution(val executable: Path, val source: String)

    /** The actionable missing-daemon refusal (never a repository fallback). */
    class MissingDaemonException(message: String) : Exception(message)

    /**
     * Resolve the daemon executable from machine-scoped inputs only. The
     * executable is only existence-checked here; checksum trust is a
     * separate decision ([checksumTrust]).
     */
    fun resolve(
        applicationBinaryPath: String?,
        home: Path,
        env: Map<String, String> = System.getenv(),
        isRunnable: (Path) -> Boolean = { Files.isRegularFile(it) && Files.isExecutable(it) }
    ): Resolution {
        val configured = applicationBinaryPath?.trim().orEmpty()
        if (configured.isNotEmpty()) {
            val path = Paths.get(configured)
            if (!isRunnable(path)) {
                throw MissingDaemonException(
                    "the application-level Faktor daemon binary path is not an executable file: " +
                        "$path; use \"Locate daemon…\" to pick the daemon or clear the application setting"
                )
            }
            return Resolution(path, "application setting")
        }
        val fromEnv = env[ENV_BIN]?.trim().orEmpty()
        if (fromEnv.isNotEmpty()) {
            val path = Paths.get(fromEnv)
            if (!isRunnable(path)) {
                throw MissingDaemonException("FAKTOR_BIN is not an executable file: $path")
            }
            return Resolution(path, "environment (FAKTOR_BIN)")
        }
        val launcher = home.resolve(INSTALL_RELATIVE).resolve(LAUNCHER_NAME)
        if (isRunnable(launcher)) {
            return Resolution(launcher, "signed install layout")
        }
        throw MissingDaemonException(
            "Faktor daemon not found. Install the daemon into the app-level layout " +
                "~/.faktor/install (the faktor-cli installer/updater target) or use " +
                "\"Locate daemon…\" to set the application-level binary path. Repository " +
                "target/ builds and workspace-scoped settings are never resolved or executed. " +
                "(probed application setting, FAKTOR_BIN and $launcher)"
        )
    }

    /**
     * The trust state of one resolved executable:
     *  - [Verified]: a checksum record exists and matches;
     *  - [Unverified]: no checksum record exists — the host must warn and
     *    require an explicit confirmation before starting;
     *  - [Refused]: a checksum record exists but does not confirm the file
     *    (mismatch or malformed record) — the daemon MUST NOT start.
     */
    sealed class Trust {
        data class Verified(val detail: String) : Trust()
        data class Unverified(val detail: String) : Trust()
        data class Refused(val detail: String) : Trust()
    }

    /**
     * Verify [executable] against `<executable>.sha256` when that record
     * exists. The record is a sha256 hex digest (optionally followed by a
     * filename, `sha256sum` style).
     */
    fun checksumTrust(executable: Path): Trust {
        val record = executable.resolveSibling(executable.fileName.toString() + CHECKSUM_SUFFIX)
        if (!Files.isRegularFile(record)) {
            return Trust.Unverified(
                "no signed checksum record exists at $record; the resolved path must be confirmed before start"
            )
        }
        val expected = try {
            String(Files.readAllBytes(record), Charsets.UTF_8)
                .trim()
                .split(Regex("\\s+"))
                .firstOrNull()
                ?.lowercase()
        } catch (e: Exception) {
            null
        }
        if (expected == null || !SHA256_HEX.matches(expected)) {
            return Trust.Refused("the checksum record $record does not carry a sha256 digest")
        }
        val actual = try {
            sha256(executable)
        } catch (e: Exception) {
            return Trust.Refused("cannot read $executable to verify it against $record: ${e.message}")
        }
        return if (actual == expected) {
            Trust.Verified("sha256 $expected matches the checksum record $record")
        } else {
            Trust.Refused(
                "checksum mismatch for $executable: record $expected, computed $actual"
            )
        }
    }

    /** One streaming sha256 hex digest (no whole-file buffer). */
    fun sha256(path: Path): String {
        val digest = MessageDigest.getInstance("SHA-256")
        Files.newInputStream(path).use { input ->
            val buffer = ByteArray(64 shl 10)
            while (true) {
                val read = input.read(buffer)
                if (read < 0) break
                digest.update(buffer, 0, read)
            }
        }
        val out = StringBuilder(64)
        for (byte in digest.digest()) {
            out.append("%02x".format(byte))
        }
        return out.toString()
    }
}
