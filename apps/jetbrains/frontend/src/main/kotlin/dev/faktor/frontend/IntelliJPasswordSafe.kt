// The real IntelliJ Platform adapters for ControlPlaneCredentials:
//
//  - [IntelliJPasswordSafeVault] maps one (service, account) pair onto
//    `CredentialAttributes(serviceName = service, userName = account)` and
//    reads/writes through `PasswordSafe`; the token value never touches a
//    product setting, the IDE configuration XML or a plain file.
//  - [IntelliJControlPlaneScopeStore] persists the NON-secret endpoint +
//    organization coordinates in the project's PropertiesComponent so the
//    PasswordSafe row can be found again.
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

    override fun getPassword(service: String, account: String): String? =
        PasswordSafe.getInstance().get(attributes(service, account))?.getPasswordAsString()

    override fun setPassword(service: String, account: String, password: String?) {
        PasswordSafe.getInstance().set(
            attributes(service, account),
            password?.let { Credentials(account, it) }
        )
    }
}

/**
 * Non-secret coordinates in the IDE's per-project properties. The endpoint
 * and organization are not credentials; the secret stays in PasswordSafe.
 */
class IntelliJControlPlaneScopeStore(private val project: Project) : ControlPlaneScopeStore {

    companion object {
        const val KEY_ENDPOINT = "faktor.controlPlane.endpoint"
        const val KEY_ORGANIZATION = "faktor.controlPlane.organization"
    }

    override fun read(): ControlPlaneScope? {
        val properties = PropertiesComponent.getInstance(project)
        val endpoint = properties.getValue(KEY_ENDPOINT) ?: return null
        val organization = properties.getValue(KEY_ORGANIZATION) ?: return null
        val scope = ControlPlaneScope(endpoint, organization)
        return scope.takeIf { it.valid() }
    }

    override fun write(scope: ControlPlaneScope) {
        val properties = PropertiesComponent.getInstance(project)
        properties.setValue(KEY_ENDPOINT, scope.endpoint)
        properties.setValue(KEY_ORGANIZATION, scope.organization)
    }
}

/**
 * The auth-session id in the IDE's per-project properties. A session id is
 * not a credential (the token is); it exists only so the daemon's strict
 * sign-out body can name the session the token must own.
 */
class IntelliJControlPlaneSessionStore(private val project: Project) : ControlPlaneSessionStore {

    companion object {
        const val KEY_SESSION = "faktor.controlPlane.session"
    }

    override fun read(): String? {
        val value = PropertiesComponent.getInstance(project).getValue(KEY_SESSION)
        return value?.takeIf { it.trim().isNotEmpty() }
    }

    override fun write(sessionId: String?) {
        val properties = PropertiesComponent.getInstance(project)
        val normalized = sessionId?.trim()?.takeIf { it.isNotEmpty() }
        if (normalized == null) {
            properties.unsetValue(KEY_SESSION)
        } else {
            properties.setValue(KEY_SESSION, normalized)
        }
    }
}
