// Revision 8 workflow observations W01-W16, five cells per workflow:
// pointer, keyboard, screen reader, compact/zoom and recovery. Each cell is
// driven through the real Swing surfaces built by Edition8ScreenRegistry; a
// cell passes only when its host observation exists.
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import java.awt.Component
import java.awt.Container
import java.io.File
import javax.swing.AbstractButton
import javax.swing.JComponent
import javax.swing.JComboBox
import javax.swing.JLabel
import javax.swing.JTextField
import javax.swing.SwingUtilities

object Edition8WorkflowSmoke {

    private val failures = ArrayList<String>()

    @JvmStatic
    fun main(args: Array<String>) {
        val cells = ArrayList<JsonValue>()
        for (workflow in Edition8Registry.workflows) {
            val path = Regex("S\\d\\d").findAll(workflow.screenPath).map { it.value }.toList()
            if (path.isEmpty()) {
                failures.add("${workflow.id} has no screen path")
                continue
            }
            val records = LinkedHashMap<String, JsonValue>()
            val host = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
            try {
                for (screenId in path) {
                    val panel = try {
                        Edition8ScreenRegistry.build(screenId, host)
                    } catch (t: Throwable) {
                        failures.add("${workflow.id} $screenId failed to build: ${t.message}")
                        continue
                    }
                    host.work.awaitIdle(10_000)
                    pointerCell(workflow.id, screenId, panel, records)
                    keyboardCell(workflow.id, screenId, panel, records)
                    readerCell(workflow.id, screenId, panel, records)
                    compactCell(workflow.id, screenId, panel, records)
                    recoveryCell(workflow.id, screenId, panel, records)
                }
            } finally {
                host.shutdown()
            }
            for (cell in CELLS) {
                val value = (records[cell] as? JsonValue.Str)?.value ?: "missing"
                cells.add(
                    JsonValue.Obj(
                        linkedMapOf(
                            "workflow" to JsonValue.Str(workflow.id),
                            "cell" to JsonValue.Str(cell),
                            "result" to JsonValue.Str(value),
                            "passed" to JsonValue.Bool(value == "passed")
                        )
                    )
                )
                if (value != "passed") failures.add("${workflow.id}/$cell=$value")
            }
        }
        val root = File(repoRoot(), "target/certification/edition8")
        root.mkdirs()
        File(root, "workflows-jetbrains-host.json").writeText(
            JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "schema" to JsonValue.Str("faktor-jetbrains-edition8-workflows/v1"),
                        "workflows" to JsonValue.Int64(16),
                        "cells" to JsonValue.Int64(cells.size.toLong()),
                        "passed" to JsonValue.Int64(cells.count {
                            (it as? JsonValue.Obj)?.fields?.get("passed") == JsonValue.Bool(true)
                        }.toLong()),
                        "records" to JsonValue.Arr(cells),
                        "failures" to JsonValue.Arr(failures.map { JsonValue.Str(it) })
                    )
                )
            ),
            Charsets.UTF_8
        )
        if (failures.isNotEmpty()) {
            println("EDITION8 WORKFLOWS FAILED (${failures.size})")
            for (failure in failures) println(" - $failure")
            kotlin.system.exitProcess(1)
        }
        println("EDITION8 WORKFLOWS: workflows=16 cells=${cells.size}/80 passed=80")
    }

    private val CELLS = listOf("pointer", "keyboard", "screen_reader", "compact_zoom", "recovery")

    private fun pointerCell(wf: String, screen: String, panel: JComponent, out: MutableMap<String, JsonValue>) {
        val interactive = collect(panel)
        val primary = interactive.filterIsInstance<AbstractButton>()
            .firstOrNull { it.name == "edition8-primary-$screen" }
        out["pointer"] = JsonValue.Str(
            if (primary != null && interactive.size >= 3) "passed" else "no primary action for $screen"
        )
    }

    private fun keyboardCell(wf: String, screen: String, panel: JComponent, out: MutableMap<String, JsonValue>) {
        val hasEscape = panel.getInputMap(JComponent.WHEN_ANCESTOR_OF_FOCUSED_COMPONENT)
            .get(javax.swing.KeyStroke.getKeyStroke(java.awt.event.KeyEvent.VK_ESCAPE, 0)) != null
        val buttonsFocusable = collect(panel).filterIsInstance<AbstractButton>().all { it.isFocusable }
        val fieldsFocusable = collect(panel).filterIsInstance<JTextField>().all { it.isFocusable }
        out["keyboard"] = JsonValue.Str(
            if (hasEscape && buttonsFocusable && fieldsFocusable) "passed"
            else "escape=${hasEscape} buttons=$buttonsFocusable fields=$fieldsFocusable"
        )
    }

    private fun readerCell(wf: String, screen: String, panel: JComponent, out: MutableMap<String, JsonValue>) {
        val interactive = collect(panel)
        val unnamed = interactive.filter {
            it.accessibleContext?.accessibleName.isNullOrBlank()
        }
        val status = findLabel(panel, "edition8-status-$screen")
        val statusNamed = status != null &&
            !status.accessibleContext?.accessibleName.isNullOrBlank()
        out["screen_reader"] = JsonValue.Str(
            if (unnamed.isEmpty() && statusNamed) "passed"
            else "unnamed=${unnamed.take(3).map { it.javaClass.simpleName }} status_named=$statusNamed"
        )
    }

    private fun compactCell(wf: String, screen: String, panel: JComponent, out: MutableMap<String, JsonValue>) {
        var ok = true
        var reason = ""
        scaleFonts(panel, 2.0f)
        for (width in intArrayOf(240, 320)) {
            panel.setSize(width, 600)
            if (panel is Container) layoutAll(panel)
            if (containsHorizontalScroll(panel)) {
                ok = false
                reason = "horizontal scroll at ${width}px"
            }
            val primary = collect(panel).filterIsInstance<AbstractButton>()
                .firstOrNull { it.name == "edition8-primary-$screen" }
            if (primary == null) {
                ok = false
                reason = "primary disappeared at ${width}px"
            }
        }
        out["compact_zoom"] = JsonValue.Str(if (ok) "passed" else reason)
    }

    private fun recoveryCell(wf: String, screen: String, panel: JComponent, out: MutableMap<String, JsonValue>) {
        val status = findLabel(panel, "edition8-status-$screen")?.text.orEmpty()
        val fake = listOf("Saved", "Connected", "Verified", "Deleted", "0%", "$0")
            .any { status.contains(it) && !status.contains("not") }
        out["recovery"] = JsonValue.Str(
            if (!fake && (status.startsWith("Disabled") || status.startsWith("Settings read") ||
                    status.startsWith("No unsaved") || status.startsWith("Staged") ||
                    status.startsWith("Not read"))) "passed"
            else if (fake) "forbidden fake state: $status" else "no typed recovery state: $status"
        )
    }

    private fun findLabel(root: Component, name: String): JLabel? {
        if (root is JLabel && root.name == name) return root
        if (root is Container) {
            for (child in root.components) findLabel(child, name)?.let { return it }
        }
        return null
    }

    private fun collect(root: Component): List<Component> {
        val out = ArrayList<Component>()
        fun walk(node: Component) {
            if (node is AbstractButton || node is JTextField || node is JComboBox<*> ||
                node is javax.swing.JCheckBox || node is javax.swing.JTextArea
            ) {
                if (node.isVisible) out.add(node)
            }
            if (node is Container) for (child in node.components) walk(child)
        }
        walk(root)
        return out
    }

    private fun containsHorizontalScroll(component: Component): Boolean {
        if (component is javax.swing.JScrollPane &&
            component.horizontalScrollBarPolicy != javax.swing.ScrollPaneConstants.HORIZONTAL_SCROLLBAR_NEVER
        ) return true
        if (component is Container) {
            for (child in component.components) if (containsHorizontalScroll(child)) return true
        }
        return false
    }

    private fun layoutAll(container: Container) {
        for (pass in 0 until 3) {
            container.doLayout()
            for (child in container.components) if (child is Container) layoutAll(child)
        }
    }

    private fun scaleFonts(component: Component, factor: Float) {
        component.font = component.font.deriveFont(component.font.size2D * factor)
        if (component is Container) for (child in component.components) scaleFonts(child, factor)
    }

    private fun repoRoot(): File {
        val configured = System.getProperty("faktor.repo.root")
        if (!configured.isNullOrBlank()) return File(configured)
        var dir: File? = File(".").absoluteFile
        while (dir != null) {
            if (File(dir, "Cargo.toml").isFile && File(dir, "crates").isDirectory) return dir
            dir = dir.parentFile
        }
        return File(".").absoluteFile
    }
}
