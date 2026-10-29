// The daemon's data-dir REUSE contract (the host half of
// `crates/cli/src/daemon/serve_lock.rs`): while a `faktor serve` owns a data
// dir it keeps `<data-dir>/serve.lock` (`{"pid":N,"port":P}`) and
// `<data-dir>/serve.password` (0600, the serve password) on disk. A LATER
// host process (an IDE reload, a second window) health-checks that live
// holder and REUSES it instead of spawning a competitor — a duplicate
// serve would refuse with the frozen `faktor already serving` line and exit
// 3, leaving the host with a confusing failure and the data dir with two
// processes contending for one store.
//
// [BackendEndpoint] is the minimal connection surface both hosts share: the
// spawned child ([BackendConnection]) and an ADOPTED live holder
// ([AdoptedEndpoint], no child process is owned — the spawn owner keeps
// it). Adoption is only a parse here; liveness is always health-decided.
package dev.faktor.backend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.view
import java.nio.file.Files
import java.nio.file.Path

/** The ownership record inside the data dir (serve.lock). */
data class ServeLockRecord(val pid: Long, val port: Int)

/** The minimal daemon endpoint every host connection shares. */
interface BackendEndpoint {
    val port: Int
    val password: String
    val baseUrl: String

    /** The serving daemon pid when known (never a stale launcher pid). */
    fun pid(): Long

    /** True while the daemon answers /native/health with this password. */
    fun isAlive(): Boolean
}

/**
 * An ALREADY-SERVING daemon adopted through the reuse contract: no child
 * process is owned by this host, liveness is health-only, and stop() of the
 * adopting host detaches (the spawn owner keeps the daemon).
 */
class AdoptedEndpoint(
    override val port: Int,
    override val password: String,
    private val daemonPid: Long?
) : BackendEndpoint {
    override val baseUrl: String = "http://127.0.0.1:$port"

    override fun pid(): Long = daemonPid ?: 0L

    override fun isAlive(): Boolean = try {
        NativeClient(baseUrl, password, timeoutMs = 2_000L).health().ok
    } catch (e: Exception) {
        false
    }
}

/** Parses the on-disk reuse contract of one data dir (never probes). */
object ServeReuse {

    /** The lock file name (must stay identical to the daemon's). */
    const val LOCK_FILE = "serve.lock"

    /** The reattach credential file name (mode 0600, daemon-written). */
    const val PASSWORD_FILE = "serve.password"

    /** The parsed ownership record, or null when absent/malformed. */
    fun readLock(dataDir: Path): ServeLockRecord? {
        val path = dataDir.resolve(LOCK_FILE)
        if (!Files.isRegularFile(path)) return null
        val raw = try {
            String(Files.readAllBytes(path), Charsets.UTF_8)
        } catch (e: Exception) {
            return null
        }
        val view = try {
            JsonCodec.parse(raw).view(LOCK_FILE)
        } catch (e: Exception) {
            return null
        }
        val pid = view.optionalField("pid")?.long() ?: return null
        val port = view.optionalField("port")?.long() ?: return null
        if (pid <= 0L || pid > UInt.MAX_VALUE.toLong() || port <= 0L || port > 65_535L) {
            return null
        }
        return ServeLockRecord(pid, port.toInt())
    }

    /** The on-disk reattach password, or null when absent/unreadable. */
    fun readPassword(dataDir: Path): String? {
        val path = dataDir.resolve(PASSWORD_FILE)
        if (!Files.isRegularFile(path)) return null
        return try {
            String(Files.readAllBytes(path), Charsets.UTF_8).trim().takeIf { it.isNotEmpty() }
        } catch (e: Exception) {
            null
        }
    }

    /**
     * The parsed adoption candidate of [dataDir] (lock + password), or null
     * when the data dir records no complete contract. Liveness is NOT
     * checked here: callers probe health and fall back to a fresh spawn on
     * a dead/stale holder.
     */
    fun adoptedEndpoint(dataDir: Path): AdoptedEndpoint? {
        val record = readLock(dataDir) ?: return null
        val password = readPassword(dataDir) ?: return null
        return AdoptedEndpoint(record.port, password, record.pid)
    }
}
