// The REAL IntelliJ Platform host journey (audit: an actual IDE-level journey,
// not only offscreen rendering).
//
// This runs INSIDE the platform test application: a real IDEA application
// instance, a real ProjectManager project, the plugin.xml-registered Faktor
// tool window, and the production FaktorToolWindowFactory -> FaktorChatPanel.
// The journey opens the project, shows the tool window, hands focus to the ONE
// composer, types through the composer's own typed-character editor action
// (the action the Swing keymap dispatches for KEY_TYPED), dispatches the exact
// action bound to Ctrl+Enter, and writes evidence to
// target/certification/jetbrains-ide-journey/ (journey.json + a render of the
// real platform-created component tree).
//
// What this proves: the shipped plugin wiring runs inside the IDE platform
// (extensions registered, tool window created, composer reachable, keymap
// actions wired) and the send path captures the typed goal as its immutable
// submission.
// What it does NOT prove (recorded in the artifact's `does_not_prove`): OS
// focus ownership (the platform test harness has no showing window; the focus
// REQUEST is the observable) and daemon connectivity (no daemon binary is
// configured for this lane; the send's transport failure is expected and does
// not fail the journey).
//
// Selected only by the `ideJourney` Gradle task / apps/jetbrains/ide-journey.sh
// (via -Dfaktor.ide.journey=1); a plain `:frontend:test` run prints
// `IDE JOURNEY SKIPPED` and never claims the journey.
package dev.faktor.frontend

import com.intellij.openapi.Disposable
import com.intellij.openapi.application.ApplicationInfo
import com.intellij.openapi.util.Disposer
import com.intellij.openapi.wm.ToolWindowAnchor
import com.intellij.openapi.wm.ToolWindowEP
import com.intellij.openapi.wm.ToolWindowManager
import com.intellij.testFramework.fixtures.BasePlatformTestCase
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import java.awt.image.BufferedImage
import java.io.File
import java.util.concurrent.atomic.AtomicReference
import javax.imageio.ImageIO
import javax.swing.SwingUtilities

class IdeJourneyTest : BasePlatformTestCase() {

    fun testOpenProjectShowToolWindowFocusComposerTypeAndSend() {
        if (System.getProperty("faktor.ide.journey") != "1") {
            println(
                "IDE JOURNEY SKIPPED: not selected; run :frontend:ideJourney " +
                    "or apps/jetbrains/ide-journey.sh"
            )
            return
        }
        val steps = ArrayList<String>()
        fun step(line: String) {
            steps.add(line)
            println("IDE JOURNEY: $line")
        }

        // 1. The real project the platform opened for this test.
        val project = project
        step("project open: name=${project.name} basePath=${project.basePath}")

        // 2. The tool window declared by plugin.xml. The platform test harness
        // uses the headless ToolWindowManager, so the journey registers the
        // plugin.xml ToolWindowEP bean through the platform's public
        // registerToolWindow task API -- the same declaration + factory the
        // real IDE manager consumes.
        val manager = ToolWindowManager.getInstance(project)
        val bean = ToolWindowEP.EP_NAME.extensionList.firstOrNull { it.id == "Faktor" }
            ?: error(
                "plugin.xml must expose the Faktor ToolWindowEP; registered: " +
                    ToolWindowEP.EP_NAME.extensionList.map { it.id }
            )
        val factory = bean.getToolWindowFactory(bean.pluginDescriptor)
        val toolWindow = manager.getToolWindow("Faktor") ?: onEdt {
            manager.registerToolWindow("Faktor") {
                anchor = ToolWindowAnchor.fromText(bean.anchor)
                canCloseContent = bean.canCloseContents
                contentFactory = factory
            }
        }
        onEdt { toolWindow.show() }
        step(
            "tool window shown: registered id=Faktor " +
                "(headless ToolWindow reports id='${toolWindow.id}') visible=${toolWindow.isVisible}"
        )

        // 3. Content through the production factory, on the EDT like the IDE.
        onEdt {
            if (toolWindow.contentManager.contentCount == 0) {
                factory.createToolWindowContent(project, toolWindow)
            }
        }
        val content = toolWindow.contentManager.contents.singleOrNull()
            ?: error("the tool window must hold exactly one content")
        val panel = content.component as? FaktorChatPanel
            ?: error("the tool-window content must be the FaktorChatPanel, was ${content.component.javaClass}")
        Disposer.register(testRootDisposable, Disposable { panel.shutdown() })
        step("tool-window content: title=${content.displayName} component=${panel.javaClass.name}")

        // 4. Focus handoff to the ONE composer. The request routes to the
        // composer component; AWT ownership needs a showing OS window, which
        // the platform test harness does not have, so it is reported honestly
        // instead of asserted.
        val focusGranted = onEdt { panel.requestComposerFocusForTest() }
        step(
            "composer focus: request routed to the Work composer; AWT ownership granted=$focusGranted"
        )

        // 5. Typing through the composer's own typed-character action.
        val prompt = "Investigate the failing test and propose a fix"
        onEdt { panel.typeComposerForTest(prompt) }
        assertEquals(prompt, panel.goalTextForTest())
        step("composer typed: \"${panel.goalTextForTest()}\"")

        // 6. Send through the exact action bound to Ctrl+Enter.
        val sent = AtomicReference<Pair<String, String>?>()
        panel.taskStartObserver = { id, goal -> sent.set(id to goal) }
        onEdt { panel.triggerRunTaskActionForTest() }
        val observed = sent.get()
            ?: error("the Ctrl+Enter action must reach the task-start path")
        assertEquals(prompt, observed.second)
        step("send dispatched: submission=${observed.first} goal=\"${observed.second}\"")

        // 7. Evidence artifacts: the journey report + a render of the real
        // platform-created component tree.
        val artifacts = journeyArtifactsDir()
        artifacts.mkdirs()
        val png = File(artifacts, "faktor-tool-window.png")
        onEdt {
            ParityAwt.layout(panel, 900, 600)
            val image = BufferedImage(900, 600, BufferedImage.TYPE_INT_RGB)
            val g = image.createGraphics()
            try {
                panel.paint(g)
            } finally {
                g.dispose()
            }
            ImageIO.write(image, "png", png)
        }
        step("screenshot artifact: ${png.absolutePath} (${png.length()} bytes)")

        val report = obj(
            "schema" to JsonValue.Str("faktor-jetbrains-ide-journey/v1"),
            "status" to JsonValue.Str("passed"),
            "ide" to JsonValue.Str(ApplicationInfo.getInstance().build.asString()),
            "jdk" to JsonValue.Str(System.getProperty("java.version", "unknown")),
            "project" to JsonValue.Str(project.basePath ?: project.name),
            "tool_window" to obj(
                "id" to JsonValue.Str("Faktor"),
                "content_title" to JsonValue.Str(content.displayName)
            ),
            "focus_ownership_granted" to JsonValue.Bool(focusGranted),
            "sent" to obj(
                "submission_id" to JsonValue.Str(observed.first),
                "goal" to JsonValue.Str(observed.second)
            ),
            "screenshot" to JsonValue.Str(png.name),
            "does_not_prove" to JsonValue.Arr(
                listOf(
                    JsonValue.Str(
                        "OS-level focus ownership (the platform test harness has no showing window)"
                    ),
                    JsonValue.Str(
                        "daemon connectivity (no daemon binary is configured for this lane; " +
                            "the send's transport failure is expected)"
                    )
                )
            ),
            "steps" to JsonValue.Arr(steps.map { JsonValue.Str(it) })
        )
        val reportFile = File(artifacts, "journey.json")
        reportFile.writeText(JsonCodec.write(report) + "\n", Charsets.UTF_8)
        step("journey artifact: ${reportFile.absolutePath}")
    }

    private fun journeyArtifactsDir(): File =
        File(ParityPath.repoRoot(), "target/certification/jetbrains-ide-journey")

    private fun <T> onEdt(block: () -> T): T {
        if (SwingUtilities.isEventDispatchThread()) return block()
        val result = AtomicReference<T>()
        val thrown = AtomicReference<Throwable>()
        SwingUtilities.invokeAndWait {
            try {
                result.set(block())
            } catch (t: Throwable) {
                thrown.set(t)
            }
        }
        thrown.get()?.let { throw it }
        return result.get()
    }
}
