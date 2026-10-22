// Revision 8 UX audit evidence: renders every screen at the host width,
// records visible-control/duplication/overlap metrics and a per-screen
// heuristic score. This is audit tooling, not a certification gate.
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import java.awt.Component
import java.awt.Container
import java.awt.Rectangle
import java.awt.image.BufferedImage
import java.io.File
import javax.imageio.ImageIO
import javax.swing.AbstractButton
import javax.swing.JLabel
import javax.swing.JTextField
import javax.swing.SwingUtilities

object Edition8UxAudit {

    @JvmStatic
    fun main(args: Array<String>) {
        val out = File(args.firstOrNull() ?: "target/ux-audit")
        val shots = File(out, "screens")
        shots.mkdirs()
        val records = ArrayList<JsonValue>()
        val scores = LinkedHashMap<String, JsonValue>()
        val matrix = System.getenv("FAKTOR_UX_MATRIX") == "1"
        for (screen in Edition8Registry.screens) {
            val host = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
            var panel: Edition8ScreenPanel? = null
            try {
                SwingUtilities.invokeAndWait {
                    panel = Edition8ScreenRegistry.build(screen.id, host) as Edition8ScreenPanel
                }
                host.work.awaitIdle(10_000)
                val view = panel ?: continue
                layout(view, 480, 800)
                paint(view, 480, 800, File(shots, "${screen.id}-480.png"))
                val metrics = measure(view, screen.id)
                val score = score(metrics)
                if (matrix && screen.id in MATRIX_SCREENS) {
                    for (width in intArrayOf(240, 320)) {
                        val fresh = Edition8ScreenRegistry.build(screen.id, host) as Edition8ScreenPanel
                        host.work.awaitIdle(10_000)
                        layout(fresh, width, 600)
                        paint(fresh, width, 600, File(shots, "${screen.id}-$width.png"))
                    }
                    val zoomed = Edition8ScreenRegistry.build(screen.id, host) as Edition8ScreenPanel
                    host.work.awaitIdle(10_000)
                    scaleFonts(zoomed, 2.0f)
                    layout(zoomed, 320, 600)
                    paint(zoomed, 320, 600, File(shots, "${screen.id}-320-200.png"))
                }
                records.add(
                    JsonValue.Obj(
                        linkedMapOf(
                            "id" to JsonValue.Str(screen.id),
                            "title" to JsonValue.Str(screen.title),
                            "group" to JsonValue.Str(screen.group),
                            "metrics" to metrics,
                            "score" to JsonValue.Obj(
                                linkedMapOf(
                                    "clarity" to JsonValue.Int64(score[0].toLong()),
                                    "density" to JsonValue.Int64(score[1].toLong()),
                                    "consistency" to JsonValue.Int64(score[2].toLong()),
                                    "efficiency" to JsonValue.Int64(score[3].toLong()),
                                    "recovery" to JsonValue.Int64(score[4].toLong())
                                )
                            )
                        )
                    )
                )
                scores[screen.id] = JsonValue.Obj(
                    linkedMapOf(
                        "clarity" to JsonValue.Int64(score[0].toLong()),
                        "density" to JsonValue.Int64(score[1].toLong()),
                        "consistency" to JsonValue.Int64(score[2].toLong()),
                        "efficiency" to JsonValue.Int64(score[3].toLong()),
                        "recovery" to JsonValue.Int64(score[4].toLong())
                    )
                )
            } finally {
                host.shutdown()
            }
        }
        val all = records.mapNotNull { record ->
            val obj = record as? JsonValue.Obj ?: return@mapNotNull null
            (obj.fields["score"] as? JsonValue.Obj)?.fields
        }
        fun avg(key: String): Double =
            all.mapNotNull { (it[key] as? JsonValue.Int64)?.value }.average().takeUnless { it.isNaN() } ?: 0.0
        val summary = linkedMapOf<String, JsonValue>(
            "schema" to JsonValue.Str("faktor-jetbrains-edition8-ux-audit/v1"),
            "screens" to JsonValue.Int64(records.size.toLong()),
            "meanClarity" to JsonValue.Dbl(avg("clarity")),
            "meanDensity" to JsonValue.Dbl(avg("density")),
            "meanConsistency" to JsonValue.Dbl(avg("consistency")),
            "meanEfficiency" to JsonValue.Dbl(avg("efficiency")),
            "meanRecovery" to JsonValue.Dbl(avg("recovery")),
            "records" to JsonValue.Arr(records)
        )
        File(out, "ux-metrics.json").writeText(JsonCodec.write(JsonValue.Obj(summary)), Charsets.UTF_8)
        val csv = StringBuilder("screen,title,group,clarity,density,consistency,efficiency,recovery," +
            "visibleControls,duplicateActions,headerOverlap,status\n")
        for (record in records) {
            val obj = record as JsonValue.Obj
            val id = (obj.fields["id"] as JsonValue.Str).value
            val title = (obj.fields["title"] as JsonValue.Str).value
            val group = (obj.fields["group"] as JsonValue.Str).value
            val score = obj.fields["score"] as JsonValue.Obj
            val metrics = obj.fields["metrics"] as JsonValue.Obj
            fun n(key: String) = ((score.fields[key] as JsonValue.Int64).value)
            fun m(key: String) = (metrics.fields[key] as JsonValue.Int64).value
            val status = (metrics.fields["status"] as JsonValue.Str).value
            csv.append("$id,\"$title\",\"$group\",${n("clarity")},${n("density")},${n("consistency")},")
                .append("${n("efficiency")},${n("recovery")},${m("visibleControls")},")
                .append("${m("duplicateActions")},${m("headerOverlap")},\"${status.replace("\"", "'")}\"\n")
        }
        File(out, "ux-scores.csv").writeText(csv.toString(), Charsets.UTF_8)
        println(
            "EDITION8 UX AUDIT: screens=${records.size} " +
                "mean=${"%.2f".format((avg("clarity") + avg("density") + avg("consistency") +
                    avg("efficiency") + avg("recovery")) / 5.0)} " +
                "out=${out.absolutePath}"
        )
    }

    private fun measure(view: Edition8ScreenPanel, id: String): JsonValue.Obj {
        val visible = ArrayList<Component>()
        collectVisible(view, visible)
        val buttons = visible.filterIsInstance<AbstractButton>()
        val buttonTexts = buttons.mapNotNull { it.text?.trim()?.takeIf(String::isNotEmpty) }
        val duplicates = buttonTexts.groupingBy { it }.eachCount().filterValues { it > 1 }.values.sum() -
            buttonTexts.groupingBy { it }.eachCount().count { it.value > 1 }
        val primary = buttons.firstOrNull { it.name == "edition8-primary-$id" }
        val primaryRect = primary?.let { absolute(it, view) }
        val header = findNamed(view, "edition8-header-$id")
        val headerLabels = ArrayList<JLabel>()
        if (header != null) collectLabels(header, headerLabels)
        val headerOverlap = if (primaryRect == null) 0 else {
            var overlap = 0
            for (label in headerLabels) {
                if (label.text.isNullOrBlank() || label === primary) continue
                val rect = absolute(label, view)
                val shared = rect.intersection(primaryRect)
                if (!shared.isEmpty && label.text.length > 2) overlap++
            }
            overlap
        }
        val jargon = visibleLabels(view).count {
            it.contains("Read-only contract") || it.contains("Controls (") ||
                it.contains("design p.") || it.contains("contract p.") ||
                it.contains("OK / Cancel / Apply") || it.contains("apply timing")
        }
        val primaryVisible = primary != null && primary.isVisible && primary.width > 0
        val apply = buttons.firstOrNull { it.name == "edition8-apply-$id" }
        val applyVisible = apply != null && apply.isVisible
        val applyEnabled = applyVisible && apply?.isEnabled == true
        val hasPrimary = Edition8Copy.hasPrimary(view.screen)
        val hasUnsaved = view.rowsForTest().any {
            it.stateText().startsWith("Staged —") || it.stateText().startsWith("Staged ")
        }
        return JsonValue.Obj(
            linkedMapOf(
                "visibleControls" to JsonValue.Int64(visible.count {
                    it is AbstractButton || it is JTextField
                }.toLong()),
                "visibleButtons" to JsonValue.Int64(buttons.size.toLong()),
                "duplicateActions" to JsonValue.Int64(duplicates.toLong()),
                "headerOverlap" to JsonValue.Int64(headerOverlap.toLong()),
                "jargonLabels" to JsonValue.Int64(jargon.toLong()),
                "primaryVisible" to JsonValue.Int64(if (primaryVisible) 1 else 0),
                "applyVisible" to JsonValue.Int64(if (applyVisible) 1 else 0),
                "applyEnabled" to JsonValue.Int64(if (applyEnabled) 1 else 0),
                "hasPrimary" to JsonValue.Int64(if (hasPrimary) 1 else 0),
                "applyWithoutChanges" to JsonValue.Int64(if (applyEnabled && !hasUnsaved) 1 else 0),
                "status" to JsonValue.Str(view.statusForTest())
            )
        )
    }

    private fun score(m: JsonValue.Obj): IntArray {
        fun n(key: String) = ((m.fields[key] as? JsonValue.Int64)?.value ?: 0L).toInt()
        val controls = n("visibleControls")
        val duplicates = n("duplicateActions")
        val overlap = n("headerOverlap")
        val jargon = n("jargonLabels")
        val primaryVisible = n("primaryVisible") == 1
        val applyVisible = n("applyVisible") == 1
        val hasPrimary = n("hasPrimary") == 1
        val effectivePrimary = primaryVisible || applyVisible
        val applyWithoutChanges = n("applyWithoutChanges") == 1

        var clarity = 5
        if (overlap > 0) clarity -= 3
        if (duplicates > 1) clarity -= 1
        if (jargon > 0) clarity -= 1

        val density = when {
            controls in 3..7 -> 5
            controls in 8..9 -> 4
            controls in 10..12 -> 3
            controls in 13..16 -> 2
            else -> 1
        }

        var consistency = 5
        if (duplicates > 0) consistency -= 1
        if (jargon > 0) consistency -= 1
        if (applyVisible && primaryVisible) consistency -= 1

        var efficiency = 5
        if (hasPrimary && !effectivePrimary) efficiency -= 3
        if (applyVisible && primaryVisible) efficiency -= 1
        if (controls > 10) efficiency -= 1

        var recovery = 5
        if (hasPrimary && !effectivePrimary) recovery -= 1
        if (applyWithoutChanges) recovery -= 1
        if (jargon > 0) recovery -= 1
        return intArrayOf(
            clarity.coerceIn(1, 5),
            density.coerceIn(1, 5),
            consistency.coerceIn(1, 5),
            efficiency.coerceIn(1, 5),
            recovery.coerceIn(1, 5)
        )
    }

    private fun collectLabels(root: Component, out: MutableList<JLabel>) {
        if (!root.isVisible) return
        if (root is JLabel) out.add(root)
        if (root is Container) for (child in root.components) collectLabels(child, out)
    }

    private val MATRIX_SCREENS = setOf("S01", "S03", "S14", "S29", "S34", "S36", "S58", "S70")

    private fun scaleFonts(component: Component, factor: Float) {
        (component as? javax.swing.JComponent)?.let { c ->
            c.font = c.font?.deriveFont(c.font.size2D * factor)
        }
        if (component is Container) for (child in component.components) scaleFonts(child, factor)
    }

    private fun findNamed(root: Component, name: String): Component? {
        if (root.name == name) return root
        if (root is Container) for (child in root.components) findNamed(child, name)?.let { return it }
        return null
    }

    private fun visibleLabels(root: Component): List<String> {
        val labels = ArrayList<String>()
        fun walk(node: Component) {
            if (!node.isVisible) return
            if (node is JLabel) node.text?.let { labels.add(it) }
            if (node is Container) for (child in node.components) walk(child)
        }
        walk(root)
        return labels
    }

    private fun collectVisible(node: Component, out: MutableList<Component>) {
        if (!node.isVisible) return
        if (node is AbstractButton || node is JTextField || node is JLabel) out.add(node)
        if (node is Container) for (child in node.components) collectVisible(child, out)
    }

    private fun absolute(component: Component, root: Component): Rectangle {
        val rect = component.bounds
        return SwingUtilities.convertRectangle(component.parent ?: root, rect, root)
    }

    private fun layout(container: Container, width: Int, height: Int) {
        container.setSize(width, height)
        repeat(3) {
            container.doLayout()
            layoutChildren(container)
        }
    }

    private fun layoutChildren(container: Container) {
        for (child in container.components) {
            if (child is Container) {
                child.doLayout()
                layoutChildren(child)
            }
        }
    }

    private fun paint(panel: Component, width: Int, height: Int, file: File) {
        val image = BufferedImage(width, height, BufferedImage.TYPE_INT_RGB)
        val graphics = image.createGraphics()
        try {
            panel.paint(graphics)
        } finally {
            graphics.dispose()
        }
        ImageIO.write(image, "png", file)
    }
}
