// Control-plane credential storage over the IDE's PasswordSafe.
//
// The control token (`x-faktor-control-token`) is never a product setting: it
// lives in the platform credential store under credential attributes whose
// service name is [ControlPlaneCredentialStore.CREDENTIAL_SERVICE] and whose
// account is "<endpoint>|<organization>" (both non-secret coordinates, so the
// password-row identity is auditable without reading the secret).
//
// This file is platform-free on purpose: [SecretVault] mirrors the minimal
// PasswordSafe surface and [ControlPlaneScopeStore] persists the non-secret
// coordinates, so the kotlinc smoke drives every path with fake rows while
// the real IntelliJ wiring lives in IntelliJPasswordSafe.kt.
package dev.faktor.frontend

/** One non-secret coordinate pair; the credential value never enters it. */
data class ControlPlaneScope(val endpoint: String, val organization: String) {

    fun normalizedEndpoint(): String = endpoint.trim().trimEnd('/').toLowerCase()

    fun valid(): Boolean = normalizedEndpoint().isNotEmpty() && organization.trim().isNotEmpty()

    /** The PasswordSafe account half of the credential attributes. */
    fun account(): String = normalizedEndpoint() + "|" + organization.trim()
}

/**
 * The minimal PasswordSafe-shaped vault. The production adapter maps one
 * (service, account) pair onto `CredentialAttributes(service, account)`.
 */
interface SecretVault {
    fun getPassword(service: String, account: String): String?

    fun setPassword(service: String, account: String, password: String?)
}

/** Persists the NON-secret (endpoint, organization) coordinates. */
interface ControlPlaneScopeStore {
    fun read(): ControlPlaneScope?

    fun write(scope: ControlPlaneScope)
}

/**
 * Persists the NON-secret auth-session id the sign-out route must name. An
 * opaque session token cannot be reverse-mapped to its session id, so the id
 * (from the SSO callback / bootstrap result) is provisioned with the
 * credential; the token itself never touches this store.
 */
interface ControlPlaneSessionStore {
    fun read(): String?

    fun write(sessionId: String?)
}

/**
 * The credential authority: store/resolve/delete by (endpoint, organization).
 * Blank credentials are refused loudly and never written; a resolve returns
 * null (no principal) instead of fabricating one.
 */
class ControlPlaneCredentialStore(private val vault: SecretVault) {

    companion object {
        /** Credential-attributes service name for the control-plane token. */
        const val CREDENTIAL_SERVICE = "Faktor Control Plane"
    }

    fun resolve(scope: ControlPlaneScope?): String? {
        if (scope == null || !scope.valid()) return null
        val stored = vault.getPassword(CREDENTIAL_SERVICE, scope.account()) ?: return null
        return stored.takeIf { it.isNotEmpty() }
    }

    fun store(scope: ControlPlaneScope, token: String) {
        if (!scope.valid()) {
            throw IllegalArgumentException(
                "control-plane scope requires a non-empty endpoint and organization"
            )
        }
        if (token.trim().isEmpty()) {
            throw IllegalArgumentException("refusing to store an empty control-plane credential")
        }
        vault.setPassword(CREDENTIAL_SERVICE, scope.account(), token)
    }

    /** Removes the credential row; returns true when one existed. */
    fun delete(scope: ControlPlaneScope?): Boolean {
        if (scope == null || !scope.valid()) return false
        val existed = resolve(scope) != null
        vault.setPassword(CREDENTIAL_SERVICE, scope.account(), null)
        return existed
    }
}

/**
 * Change watcher for the credential row behind the (endpoint, organization)
 * coordinates.
 *
 * The IntelliJ platform's `PasswordSafe` exposes NO credential-row change
 * event (only provider-settings listeners), so an EXTERNAL write — the OS
 * keychain UI, another IDE instance, a settings import — is observed by
 * re-reading the vault on a bounded interval. Only an actual value change
 * reaches [onTokenChanged]; a removed row hands `null`, which clears the old
 * token on the live client instead of leaving it usable until restart. The
 * value is never logged, persisted or copied anywhere but the callback.
 *
 * The watcher is deterministic for tests: [pollOnce] performs exactly one
 * resolution and [start] only adds the bounded background loop. The FIRST
 * poll APPLIES the resolved value (reconciling a client that was built before
 * the watch started, so an external update in that window is not missed);
 * every later poll fires only on an actual change.
 */
class ControlPlaneCredentialWatcher(
    private val store: ControlPlaneCredentialStore,
    private val scopeStore: ControlPlaneScopeStore,
    private val onTokenChanged: (String?) -> Unit,
    intervalMs: Long = DEFAULT_INTERVAL_MS
) {
    companion object {
        /** The bounded re-read interval; the OS store has no change event. */
        const val DEFAULT_INTERVAL_MS = 5_000L

        /** The smallest accepted interval, so a caller cannot spin. */
        const val MIN_INTERVAL_MS = 250L
    }

    private val lock = Any()

    private var thread: Thread? = null

    private var last: String? = null

    private var initialized = false

    private val safeIntervalMs: Long = intervalMs.coerceAtLeast(MIN_INTERVAL_MS)

    fun start() {
        synchronized(lock) {
            if (thread != null) return
            val t = Thread({ runLoop() }, "faktor-control-plane-credential-watch")
            t.isDaemon = true
            thread = t
            t.start()
        }
    }

    fun stop() {
        val t = synchronized(lock) {
            val current = thread
            thread = null
            current
        }
        if (t != null) {
            t.interrupt()
            try {
                t.join(500)
            } catch (e: InterruptedException) {
                Thread.currentThread().interrupt()
            }
        }
    }

    /**
     * One synchronous poll; returns true when the resolved token was APPLIED
     * (the first poll applies so a client built before the watch started is
     * reconciled; later polls apply only on an actual change).
     */
    fun pollOnce(): Boolean {
        val current = store.resolve(scopeStore.read())
        val changed = synchronized(lock) {
            val differs = !initialized || current != last
            last = current
            initialized = true
            differs
        }
        if (changed) {
            onTokenChanged(current)
        }
        return changed
    }

    private fun runLoop() {
        while (true) {
            try {
                pollOnce()
            } catch (e: Exception) {
                // A vault read failure never kills the watch and never
                // fabricates a token; the previous value stays live and the
                // next poll retries.
            }
            try {
                Thread.sleep(safeIntervalMs)
            } catch (e: InterruptedException) {
                return
            }
        }
    }
}
