// APPLICATION-level Faktor daemon settings for the IntelliJ host.
//
// The Faktor binary path is an APPLICATION-scoped setting (the IDE's
// application-level properties store, `options/other.xml` in the IDE config
// directory): no project, module or workspace can supply it, so a repository
// can never point the plugin at its own executable. The daemon itself is
// installed into the app-level layout ~/.faktor/install (managed by the
// faktor-cli installer/updater) and resolved through its stable bootstrap
// launcher.
//
// This is the only IntelliJ-platform file for daemon settings; the pure
// resolution/trust rules live in dev.faktor.backend.DaemonBinary so they are
// smoke-testable without the platform.
package dev.faktor.frontend

import com.intellij.ide.util.PropertiesComponent
import com.intellij.openapi.fileChooser.FileChooser
import com.intellij.openapi.fileChooser.FileChooserDescriptorFactory
import com.intellij.openapi.project.Project
import com.intellij.openapi.ui.Messages
import dev.faktor.backend.DaemonBinary
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths

object FaktorDaemonSettings {

    /** The application-level properties key (never a project property). */
    const val BINARY_PATH_KEY = "faktor.daemon.binaryPath"

    /** The application-level Faktor binary path, or null when unset. */
    fun applicationBinaryPath(): String? =
        PropertiesComponent.getInstance().getValue(BINARY_PATH_KEY)?.takeIf { it.isNotBlank() }

    /** Persist the operator-picked daemon path at APPLICATION scope only. */
    fun storeApplicationBinaryPath(path: Path) {
        PropertiesComponent.getInstance().setValue(
            BINARY_PATH_KEY,
            path.toAbsolutePath().normalize().toString()
        )
    }

    fun clearApplicationBinaryPath() {
        PropertiesComponent.getInstance().unsetValue(BINARY_PATH_KEY)
    }

    private fun home(): Path = Paths.get(System.getProperty("user.home", "."))

    /**
     * Resolve the daemon from machine-scoped inputs only. When nothing is
     * found, report the clear, actionable error with a "Locate daemon…"
     * action; a successful locate stores the app-level setting and resolves
     * again. The unresolved refusal is rethrown so the caller's start path
     * reports exactly why nothing was started.
     */
    fun resolve(project: Project?): DaemonBinary.Resolution {
        val first = try {
            return DaemonBinary.resolve(applicationBinaryPath(), home())
        } catch (e: DaemonBinary.MissingDaemonException) {
            e
        }
        if (project != null && promptLocateDaemon(project, first.message ?: "Faktor daemon not found")) {
            return DaemonBinary.resolve(applicationBinaryPath(), home())
        }
        throw first
    }

    /**
     * The actionable missing-daemon dialog: "Locate daemon…" opens the file
     * chooser, stores the picked executable at application scope and returns
     * true; Cancel returns false.
     */
    fun promptLocateDaemon(project: Project, message: String): Boolean {
        val choice = Messages.showDialog(
            project,
            message,
            "Faktor: daemon not found",
            arrayOf("Locate daemon…", "Cancel"),
            0,
            Messages.getWarningIcon()
        )
        if (choice != 0) return false
        val descriptor = FileChooserDescriptorFactory.createSingleFileNoJarsDescriptor()
            .withTitle("Locate the faktor-cli daemon executable")
            .withDescription(
                "Pick the faktor-cli executable (the daemon). The path is stored as an " +
                    "application-level Faktor setting, never in the project."
            )
        val chosen = FileChooser.chooseFile(descriptor, project, null) ?: return false
        val path = Paths.get(chosen.path)
        if (!Files.isRegularFile(path) || !Files.isExecutable(path)) {
            Messages.showErrorDialog(
                project,
                "Not an executable file: $path",
                "Faktor: daemon not found"
            )
            return false
        }
        storeApplicationBinaryPath(path)
        return true
    }

    /**
     * Confirm one UNVERIFIED executable (no checksum record) before start:
     * the documented warn + explicit confirmation naming the exact path.
     */
    fun confirmUnverified(executable: Path, detail: String): Boolean =
        Messages.showDialog(
            null,
            "$detail\n\nStart this exact executable?\n$executable",
            "Faktor: unverified daemon binary",
            arrayOf("Start anyway", "Cancel"),
            1,
            Messages.getWarningIcon()
        ) == 0
}
