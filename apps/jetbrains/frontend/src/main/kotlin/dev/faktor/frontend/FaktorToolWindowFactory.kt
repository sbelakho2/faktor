// IntelliJ tool-window host for the existing FaktorChatPanel. The panel is
// the same Swing component the standalone launcher uses; all backend work
// still goes through FaktorFrontendService -> BackendProcessManager /
// NativeClient / NativeEventStream. This class is the only IntelliJ-platform
// dependency in the frontend module; the kotlinc fallback smoke compiles
// FaktorChatPanel/FaktorFrontendService without it.
//
// Daemon resolution is MACHINE-scoped only (FaktorDaemonSettings ->
// DaemonBinary): the application-level Faktor binary path, FAKTOR_BIN or the
// app-level signed install layout (~/.faktor/install). A project/repository
// target/ build is never consulted.
//
// Lifecycle (trusted-install parity with the VS Code supervisor): the window
// lifecycle start below auto-starts ONLY a checksum-verified executable
// inside the trusted install root — adopting a live serve.lock holder
// through the daemon's reuse contract instead of spawning a competitor.
// Every other authority state (missing binary, checksum mismatch,
// unverified bytes, a binary outside the trusted root) surfaces TYPED and
// waits for the explicit "Start service" action, which resolves (with
// "Locate daemon…" on a miss), reports the exact executable and requires an
// operator confirmation for anything unverified.
package dev.faktor.frontend

import com.intellij.openapi.Disposable
import com.intellij.openapi.project.Project
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.wm.ToolWindow
import com.intellij.openapi.wm.ToolWindowFactory
import com.intellij.ui.content.ContentFactory
import dev.faktor.backend.BackendException
import dev.faktor.backend.DaemonBinary
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
        // client; the non-secret coordinates are the SAME application-level
        // keys the platform Configurable writes, so both surfaces resolve one
        // credential row in every project.
        val credentialStore = ControlPlaneCredentialStore(IntelliJPasswordSafeVault())
        val scopeStore = IntelliJControlPlaneScopeStore()
        scopeStore.migrateFromProject(project)
        val sessionStore = IntelliJControlPlaneSessionStore()
        val service = FaktorFrontendService(
            daemonBinaryResolver(project),
            dataDir,
            controlToken = credentialStore.resolve(scopeStore.read())
        )
        // The session workspace root is the project base path: attachment
        // paths are relativized against it and session creation carries it,
        // so the daemon resolves the submitted workspace-relative `files`
        // against the same root (the projection exposes no workspace path).
        val workspaceRoot = project.basePath?.let { Paths.get(it) }
        val panel = FaktorChatPanel(
            service,
            credentialStore,
            scopeStore,
            sessionStore,
            workspaceRoot,
            FaktorSettingsController(IntelliJApplicationSettingsStore()),
            ChangeReviewAdapter(project),
            edition8HostEditor = IntelliJHostEditor(project)
        )
        val content = ContentFactory.getInstance().createContent(panel, "Faktor", false)
        Disposer.register(
            content,
            Disposable {
                panel.shutdown()
                service.stop()
            }
        )
        toolWindow.contentManager.addContent(content)
        // NO automatic daemon start from the composer, a send or any other
        // user action, and never for an untrusted binary. The window
        // LIFECYCLE hook below starts (or serve.lock-adopts) only a
        // checksum-verified executable inside the trusted install root — the
        // VS Code supervisor parity — off the EDT, with every other
        // authority state typed and waiting for the explicit
        // "Start service" action.
        startLifecycleAutoStart(service, panel)
    }

    companion object {

        /**
         * The lazy daemon resolver handed to the service: runs on the
         * explicit start action, resolves machine-scoped only, offers
         * "Locate daemon…" when nothing is found, and refuses a checksum
         * mismatch. An unverified executable (no checksum record) requires
         * the operator's explicit confirmation before the spawn.
         */
        fun daemonBinaryResolver(project: Project?): () -> Path = {
            val resolution = FaktorDaemonSettings.resolve(project)
            when (val trust = DaemonBinary.checksumTrust(resolution.executable)) {
                is DaemonBinary.Trust.Refused ->
                    throw BackendException("refusing to start the daemon: ${trust.detail}")
                is DaemonBinary.Trust.Unverified -> {
                    if (!FaktorDaemonSettings.confirmUnverified(resolution.executable, trust.detail)) {
                        throw BackendException(
                            "daemon start cancelled: ${resolution.executable} was not confirmed " +
                                "(${trust.detail})"
                        )
                    }
                }
                is DaemonBinary.Trust.Verified -> {}
            }
            resolution.executable
        }

        fun resolveDataDir(): Path {
            val env = System.getenv("FAKTOR_DATA_DIR")
            if (!env.isNullOrBlank()) return Paths.get(env)
            return Paths.get(System.getProperty("user.home"), ".faktor")
        }

        /** The dialog-free machine resolution (auto-start never prompts). */
        fun resolveQuiet(): DaemonBinary.Resolution =
            DaemonBinary.resolve(
                FaktorDaemonSettings.applicationBinaryPath(),
                Paths.get(System.getProperty("user.home", "."))
            )

        /**
         * The tool-window lifecycle auto-start: one daemon thread runs the
         * trusted-install policy against the REAL service (adoption first,
         * then the verified spawn) and surfaces the typed result on the
         * panel. [resolve]/[trustedRoot]/[service]/[panel] are injectable so
         * the shipped adapter seam is testable without a showing IDE.
         */
        internal fun startLifecycleAutoStart(
            service: FaktorFrontendService,
            panel: FaktorChatPanel,
            resolve: () -> DaemonBinary.Resolution = { resolveQuiet() },
            trustedRoot: Path = DaemonBinary.trustedInstallRoot(
                Paths.get(System.getProperty("user.home", "."))
            ),
            trustOf: (Path) -> DaemonBinary.Trust = { DaemonBinary.checksumTrust(it) }
        ) {
            val thread = Thread(
                {
                    val outcome = ToolWindowAutoStart.lifecycleAutoStart(
                        service, resolve, trustedRoot, trustOf
                    )
                    if (outcome.started) {
                        panel.hostLifecycleStarted(
                            outcome.version ?: "unknown", outcome.adopted
                        )
                    } else {
                        panel.hostLifecycleRefused(
                            outcome.refusalKind ?: ToolWindowAutoStart.KIND_STARTUP_FAILED,
                            outcome.refusalDetail ?: ""
                        )
                    }
                },
                "faktor-lifecycle-autostart"
            )
            thread.isDaemon = true
            thread.start()
        }
    }
}
