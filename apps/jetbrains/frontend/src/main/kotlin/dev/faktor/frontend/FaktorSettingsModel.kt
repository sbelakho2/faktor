// I08 — APPLICATION-level Faktor settings: one shared model/controller for
// the platform `FaktorConfigurable` and every fallback utility (the tool
// window Settings panel, FaktorDaemonSettings). The daemon binary path, the
// run defaults (provider/model/mutation mode) and the OPTIONAL control-plane
// coordinates are application-scoped values; NOTHING here is project- or
// workspace-configurable, and no credential value ever enters this model —
// control-plane tokens live ONLY in the IDE credential store (PasswordSafe).
//
// Persistence is one batch write over [FaktorSettingsStore]: validation runs
// entirely BEFORE the single [FaktorSettingsStore.write] call, so an invalid
// configuration writes nothing (no partial write). The platform side maps a
// [FaktorSettingsValidationException] onto ConfigurationException.
package dev.faktor.frontend

import java.net.URI
import java.nio.file.Files
import java.nio.file.Paths

/** Application-level settings keys (the PropertiesComponent names). */
object FaktorSettingKeys {
    const val BINARY_PATH = "faktor.daemon.binaryPath"
    const val RUN_PROVIDER = "faktor.run.defaultProvider"
    const val RUN_MODEL = "faktor.run.defaultModel"
    const val RUN_MUTATION_MODE = "faktor.run.defaultMutationMode"
    const val CONTROL_PLANE_ENDPOINT = "faktor.controlPlane.endpoint"
    const val CONTROL_PLANE_ORGANIZATION = "faktor.controlPlane.organization"
}

/**
 * The minimal application-properties seam. [write] receives the FULL,
 * already-validated batch in one call (`null` value = unset), so callers can
 * observe that a refused configuration wrote nothing and an accepted one
 * wrote exactly once.
 */
interface FaktorSettingsStore {
    fun read(key: String): String?

    fun write(values: Map<String, String?>)
}

/** One immutable application-settings snapshot (never carries a credential). */
data class FaktorSettingsSnapshot(
    val binaryPath: String?,
    val provider: String?,
    val model: String?,
    val mutationMode: String,
    /** Default EMPTY: no vendor endpoint is ever substituted. */
    val controlPlaneEndpoint: String,
    val controlPlaneOrganization: String
) {
    companion object {
        const val DEFAULT_MUTATION_MODE = "default"

        val DEFAULTS = FaktorSettingsSnapshot(
            binaryPath = null,
            provider = null,
            model = null,
            mutationMode = DEFAULT_MUTATION_MODE,
            controlPlaneEndpoint = "",
            controlPlaneOrganization = ""
        )
    }
}

/** One typed invalid-configuration refusal with a useful message. */
class FaktorSettingsValidationException(message: String) : Exception(message)

/** The exact validation rules both surfaces share (no drift possible). */
object FaktorSettingsRules {

    /** The mutation modes the runtime offers (direct-owner mode is removed). */
    val MUTATION_MODES: List<String> = listOf(
        FaktorSettingsSnapshot.DEFAULT_MUTATION_MODE,
        "shadow"
    )

    private const val MAX_TEXT = 200

    /** Non-blank values must be absolute, existing executable files. */
    fun binaryPathError(value: String?): String? {
        val path = value?.trim().orEmpty()
        if (path.isEmpty()) return null
        val parsed = try {
            Paths.get(path)
        } catch (e: Exception) {
            return "the daemon binary path is not a valid path: $path"
        }
        if (!parsed.isAbsolute) {
            return "the daemon binary path must be absolute, got: $path"
        }
        if (!Files.isRegularFile(parsed) || !Files.isExecutable(parsed)) {
            return "the daemon binary path must be an existing executable file: $path"
        }
        return null
    }

    fun providerError(value: String?): String? = textError("provider", value)

    fun modelError(value: String?): String? = textError("model", value)

    private fun textError(noun: String, value: String?): String? {
        val text = value?.trim().orEmpty()
        if (text.isEmpty()) return null
        if (text.length > MAX_TEXT) return "the $noun must be at most $MAX_TEXT characters"
        if (text.any { it < ' ' || it == '\u007f' }) {
            return "the $noun must not contain control characters"
        }
        return null
    }

    fun mutationModeError(value: String): String? =
        if (MUTATION_MODES.contains(value)) {
            null
        } else {
            "unknown mutation mode \"$value\"; offered: " + MUTATION_MODES.joinToString(", ")
        }

    /** Empty endpoint allowed (the default); non-empty must be an http(s) URL. */
    fun endpointError(value: String): String? {
        val text = value.trim()
        if (text.isEmpty()) return null
        val uri = try {
            URI(text)
        } catch (e: Exception) {
            return "the control-plane endpoint is not a valid URL: $text"
        }
        val scheme = uri.scheme?.lowercase()
        if (scheme != "http" && scheme != "https") {
            return "the control-plane endpoint must use http or https: $text"
        }
        if (uri.host.isNullOrEmpty()) {
            return "the control-plane endpoint must name a host: $text"
        }
        return null
    }

    fun organizationError(value: String): String? {
        val text = value.trim()
        if (text.isEmpty()) return null
        if (text.length > MAX_TEXT) {
            return "the control-plane organization must be at most $MAX_TEXT characters"
        }
        if (text.any { it < ' ' || it == '\u007f' }) {
            return "the control-plane organization must not contain control characters"
        }
        return null
    }

    /** The first refusal of the whole snapshot, or null when valid. */
    fun errorOf(snapshot: FaktorSettingsSnapshot): String? =
        binaryPathError(snapshot.binaryPath)
            ?: providerError(snapshot.provider)
            ?: modelError(snapshot.model)
            ?: mutationModeError(snapshot.mutationMode)
            ?: endpointError(snapshot.controlPlaneEndpoint)
            ?: organizationError(snapshot.controlPlaneOrganization)

    /** The stored form of one optional text value (blank becomes unset). */
    fun storedOrNull(value: String?): String? = value?.trim()?.takeIf { it.isNotEmpty() }
}

/**
 * The one shared settings controller. [apply] validates FIRST and then issues
 * exactly ONE batch write; [reset] returns the defaults without touching the
 * store (and therefore never the credential store, which it cannot reach).
 */
class FaktorSettingsController(private val store: FaktorSettingsStore) {

    fun read(): FaktorSettingsSnapshot = FaktorSettingsSnapshot(
        binaryPath = FaktorSettingsRules.storedOrNull(store.read(FaktorSettingKeys.BINARY_PATH)),
        provider = FaktorSettingsRules.storedOrNull(store.read(FaktorSettingKeys.RUN_PROVIDER)),
        model = FaktorSettingsRules.storedOrNull(store.read(FaktorSettingKeys.RUN_MODEL)),
        mutationMode = FaktorSettingsRules.storedOrNull(store.read(FaktorSettingKeys.RUN_MUTATION_MODE))
            ?: FaktorSettingsSnapshot.DEFAULT_MUTATION_MODE,
        controlPlaneEndpoint = store.read(FaktorSettingKeys.CONTROL_PLANE_ENDPOINT)?.trim().orEmpty(),
        controlPlaneOrganization = store.read(FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION)?.trim().orEmpty()
    )

    /** The defaults, computed without any store access. */
    fun reset(): FaktorSettingsSnapshot = FaktorSettingsSnapshot.DEFAULTS

    /** Validate + persist in one batch; an invalid snapshot writes nothing. */
    fun apply(snapshot: FaktorSettingsSnapshot) {
        val error = FaktorSettingsRules.errorOf(snapshot)
        if (error != null) throw FaktorSettingsValidationException(error)
        store.write(
            linkedMapOf(
                FaktorSettingKeys.BINARY_PATH to FaktorSettingsRules.storedOrNull(snapshot.binaryPath),
                FaktorSettingKeys.RUN_PROVIDER to FaktorSettingsRules.storedOrNull(snapshot.provider),
                FaktorSettingKeys.RUN_MODEL to FaktorSettingsRules.storedOrNull(snapshot.model),
                FaktorSettingKeys.RUN_MUTATION_MODE to snapshot.mutationMode,
                FaktorSettingKeys.CONTROL_PLANE_ENDPOINT to
                    FaktorSettingsRules.storedOrNull(snapshot.controlPlaneEndpoint),
                FaktorSettingKeys.CONTROL_PLANE_ORGANIZATION to
                    FaktorSettingsRules.storedOrNull(snapshot.controlPlaneOrganization)
            )
        )
    }

    /**
     * Persist ONLY the run defaults (the Settings utility path): the binary
     * path and control-plane coordinates — and therefore any credential —
     * are untouched. Still one validated batch write.
     */
    fun applyRunDefaults(provider: String?, model: String?, mutationMode: String) {
        val error = FaktorSettingsRules.providerError(provider)
            ?: FaktorSettingsRules.modelError(model)
            ?: FaktorSettingsRules.mutationModeError(mutationMode)
        if (error != null) throw FaktorSettingsValidationException(error)
        store.write(
            linkedMapOf(
                FaktorSettingKeys.RUN_PROVIDER to FaktorSettingsRules.storedOrNull(provider),
                FaktorSettingKeys.RUN_MODEL to FaktorSettingsRules.storedOrNull(model),
                FaktorSettingKeys.RUN_MUTATION_MODE to mutationMode
            )
        )
    }
}
