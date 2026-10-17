// The real IntelliJ Platform adapters for ControlPlaneCredentials:
//
//  - [IntelliJPasswordSafeVault] maps one (service, account) pair onto
//    `CredentialAttributes(serviceName = service, userName = account)` and
//    reads/writes through `PasswordSafe`; the token value never touches a
//    product setting, the IDE configuration XML or a plain file.
//  - [IntelliJControlPlaneScopeStore] persists the NON-secret endpoint +
//    organization coordinates in the application's PropertiesComponent (the
//    SAME keys the platform Configurable writes) so the PasswordSafe row can
//    be found again from every project.
//  - [IntelliJControlPlaneSessionStore] persists the NON-secret auth-session
//    id the sign-out route names; it is not a credential and never enters
//    PasswordSafe.
//
// Like FaktorToolWindowFactory, this file carries the IntelliJ platform
// dependency; the kotlinc smoke compiles the rest of the frontend without it.
package dev.faktor.frontend

import com.intellij.credentialStore.CredentialAttributes
import com.intellij.credentialStore.Credentials
import com.intellij.ide.passwordSafe.PasswordSafe
import com.intellij.ide.util.PropertiesComponent
import com.intellij.openapi.project.Project

/** PasswordSafe-backed vault; credential attributes = service + account. */
class IntelliJPasswordSafeVault : SecretVault {

    private fun attributes(service: String, account: String): CredentialAttributes =
        CredentialAttributes(service, account)

    // Platform 241+ exposes the service through the companion `instance`
    // property (the JVM-visible `getInstance()` static does not resolve from
    // Kotlin). `PasswordSafe.instance` is the supported Kotlin accessor.
    override fun getPassword(service: String, account: String): String? =
        PasswordSafe.instance.get(attributes(service, account))?.getPasswordAsString()

    override fun setPassword(service: String, account: String, password: String?) {
        PasswordSafe.instance.set(
            attributes(service, account),
            password?.let { Credentials(account, it) }
        )
    }
}

/**
 * Non-secret coordinates in the IDE's APPLICATION properties. These are the
 * same keys the platform Configurable writes, so a value entered in either
 * surface resolves the same PasswordSafe row in every project. The endpoint
 * and organization are not credentials; the secret stays in PasswordSafe.
 */
class IntelliJControlPlaneScopeStore : ControlPlaneScopeStore {

    companion object {
        const val KEY_ENDPOINT = FaktorSettingKeys.CONTROL_PLANE_ENDPOINT
        const val KEY_ORGANIZATION = FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION
    }

    override fun read(): ControlPlaneScope? {
        val properties = PropertiesComponent.getInstance()
        val endpoint = properties.getValue(KEY_ENDPOINT) ?: return null
        val organization = properties.getValue(KEY_ORGANIZATION) ?: return null
        val scope = ControlPlaneScope(endpoint, organization)
        return scope.takeIf { it.valid() }
    }

    override fun write(scope: ControlPlaneScope) {
        val properties = PropertiesComponent.getInstance()
        properties.setValue(KEY_ENDPOINT, scope.endpoint)
        properties.setValue(KEY_ORGANIZATION, scope.organization)
    }

    /**
     * One-shot upgrade from the former per-project coordinates: a project
     * value is copied ONLY when the shared application value is absent (an
     * explicit application choice always wins). The legacy project keys are
     * then REMOVED, exactly once: leaving them behind would let a project
     * resurrect a stale coordinate after the operator clears the application
     * value. A second call is a no-op (the keys are already gone).
     */
    fun migrateFromProject(project: Project) {
        val legacy = PropertiesComponent.getInstance(project)
        val endpoint = legacy.getValue(KEY_ENDPOINT)
        val organization = legacy.getValue(KEY_ORGANIZATION)
        if (endpoint != null && organization != null && read() == null) {
            write(ControlPlaneScope(endpoint, organization))
        }
        val sessionKey = IntelliJControlPlaneSessionStore.KEY_SESSION
        val session = legacy.getValue(sessionKey)
        if (session != null && PropertiesComponent.getInstance().getValue(sessionKey) == null) {
            IntelliJControlPlaneSessionStore().write(session)
        }
        // The shared application scope is the only authority from here on.
        // Unsetting a missing value is a no-op, so re-running is idempotent.
        legacy.unsetValue(KEY_ENDPOINT)
        legacy.unsetValue(KEY_ORGANIZATION)
        legacy.unsetValue(sessionKey)
    }
}

/**
 * The auth-session id in the IDE's APPLICATION properties (the credential it
 * names is machine-wide, so its coordinate is too). A session id is not a
 * credential (the token is); it exists only so the daemon's strict sign-out
 * body can name the session the token must own.
 */
class IntelliJControlPlaneSessionStore : ControlPlaneSessionStore {

    companion object {
        const val KEY_SESSION = FaktorSettingKeys.CONTROL_PLANE_SESSION
    }

    override fun read(): String? {
        val value = PropertiesComponent.getInstance().getValue(KEY_SESSION)
        return value?.takeIf { it.trim().isNotEmpty() }
    }

    override fun write(sessionId: String?) {
        val properties = PropertiesComponent.getInstance()
        val normalized = sessionId?.trim()?.takeIf { it.isNotEmpty() }
        if (normalized == null) {
            properties.unsetValue(KEY_SESSION)
        } else {
            properties.setValue(KEY_SESSION, normalized)
        }
    }
}
