// IntelliJ tool-window host for the existing FaktorChatPanel. The panel is
// the same Swing component the standalone launcher uses; all backend work
// still goes through FaktorFrontendService -> BackendProcessManager /
// NativeClient / NativeEventStream. This class is the only IntelliJ-platform
// dependency in the frontend module; the kotlinc fallback smoke compiles
// FaktorChatPanel/FaktorFrontendService without it.
package dev.faktor.frontend

import com.intellij.openapi.Disposable
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowFactory
import com.intellij.ui.content.ContentFactory
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths

class FaktorToolWindowFactory : ToolWindowFactory {

    override fun createToolWindowContent(project: Project, toolWindow: ToolWindow) {
        val dataDir = resolveDataDir()
        Files.createDirectories(dataDir)
        // Parity with the VS Code extension: the control-plane credential is
        // read from the OS/IDE secret store (PasswordSafe, credential
        // attributes = service + "endpoint|organization") and handed to the
        // client; the non-secret coordinates live in project properties.
        val credentialStore = ControlPlaneCredentialStore(IntelliJPasswordSafeVault())
        val scopeStore = IntelliJControlPlaneScopeStore(project)
        val sessionStore = IntelliJControlPlaneSessionStore(project)
        val service = FaktorFrontendService(
            resolveBinary(project),
            dataDir,
            controlToken = credentialStore.resolve(scopeStore.read())
        )
        val panel = FaktorChatPanel(service, credentialStore, scopeStore, sessionStore)
        val content = ContentFactory.getInstance().createContent(panel, "Faktor", false)
        Disposer.register(
            content,
            Disposable {
                panel.shutdown()
                service.stop()
            }
        )
        toolWindow.contentManager.addContent(content)
        panel.startDaemon()
    }

    companion object {
        private val BINARY_CANDIDATES = listOf(
            "target/debug/faktor-cli",
            "target/release/faktor-cli"
        )

        fun resolveBinary(project: Project): Path {
            val env = System.getenv("FAKTOR_BIN")
            if (!env.isNullOrBlank()) return Paths.get(env)
            val base = project.basePath
            if (base != null) {
                for (relative in BINARY_CANDIDATES) {
                    val candidate = Paths.get(base, relative)
                    if (Files.isExecutable(candidate)) return candidate
                }
            }
            return Paths.get(BINARY_CANDIDATES[0])
        }

        fun resolveDataDir(): Path {
            val env = System.getenv("FAKTOR_DATA_DIR")
            if (!env.isNullOrBlank()) return Paths.get(env)
            return Paths.get(System.getProperty("user.home"), ".faktor")
        }
    }
}
