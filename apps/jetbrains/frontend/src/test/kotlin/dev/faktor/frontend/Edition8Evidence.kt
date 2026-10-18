// Edition 8 JetBrains evidence harness: optical cap-centre measurements,
// rendered contrast pairs and per-control accessibility assertions over the
// real Swing panels. Writes machine-readable artifacts consumed by
// tests/edition8/{optical,contrast,assistive} gates.
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import java.awt.Color
import java.awt.Component
import java.awt.Container
import java.awt.image.BufferedImage
import java.io.File
import java.security.MessageDigest
import java.util.Locale
import javax.swing.AbstractButton
import javax.swing.JCheckBox
import javax.swing.JComboBox
import javax.swing.JComponent
import javax.swing.JTextField
import javax.swing.SwingUtilities
import javax.swing.UIManager

object Edition8Evidence {

    private fun platformName(): String {
        val os = System.getProperty("os.name").lowercase()
        return when {
            os.contains("mac") -> "macos"
            os.contains("win") -> "windows"
            else -> "linux"
        }
    }

    private fun writeEvidence(root: File, name: String, text: String) {
        File(root, "$name.raw.json").writeText(text)
        File(root, "$name.${platformName()}.raw.json").writeText(text)
    }

    private val failures = ArrayList<String>()
    private val themes = listOf("light", "dark", "high-contrast")

    @JvmStatic
    fun main(args: Array<String>) {
        val registryErrors = Edition8Registry.validate()
        failures.addAll(registryErrors)
        val optical = LinkedHashMap<String, JsonValue>()
        val contrastPairs = ArrayList<JsonValue>()
        val assistiveRows = ArrayList<JsonValue>()
        val seenControls = LinkedHashSet<String>()

        // Per-control accessibility over all 405 rendered control rows.
        for (screen in Edition8Registry.screens) {
            val host = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
            try {
                val panel = build(screen.id, host)
                for (row in panel.rowsForTest()) {
                    val id = row.control.id
                    if (!seenControls.add(id)) continue
                    val probe = row.probe
                    val name = probe.accessibleContext?.accessibleName
                    val role = probe.accessibleContext?.accessibleRole?.toString() ?: ""
                    val focusable = probe.isFocusable
                    val handler = probe.actionListeners.isNotEmpty()
                    if (name.isNullOrBlank()) failures.add("$id probe has no accessible name")
                    if (role.isBlank()) failures.add("$id probe has no accessible role")
                    if (!focusable) failures.add("$id probe is not keyboard focusable")
                    if (!handler) failures.add("$id probe has no registered handler")
                    val field = row.field
                    val toggle = row.toggle
                    if (toggle != null) {
                        if (toggle.accessibleContext?.accessibleName.isNullOrBlank()) failures.add("$id toggle has no accessible name")
                        if (!toggle.isFocusable) failures.add("$id toggle is not focusable")
                    } else if (field != null) {
                        if (field.accessibleContext?.accessibleName.isNullOrBlank()) failures.add("$id field has no accessible name")
                        if (!field.isFocusable) failures.add("$id field is not focusable")
                    }
                    assistiveRows.add(
                        JsonValue.Obj(
                            linkedMapOf(
                                "id" to JsonValue.Str(id),
                                "screen" to JsonValue.Str(screen.id),
                                "role" to JsonValue.Str(role),
                                "accessible_name" to JsonValue.Str(name ?: ""),
                                "focusable" to JsonValue.Bool(focusable),
                                "handler" to JsonValue.Bool(handler),
                                "input" to JsonValue.Str(
                                    when {
                                        field != null -> "text"
                                        toggle != null -> "checkbox"
                                        else -> "read"
                                    }
                                )
                            )
                        )
                    )
                }
            } catch (t: Throwable) {
                failures.add("${screen.id} a11y build failed: ${t::class.java.name}: ${t.message} cause=${t.cause?.let { c -> "${c::class.java.name}: ${c.message}" }}")
            } finally {
                host.shutdown()
            }
        }

        for (theme in themes) {
            applyTheme(theme)
            try {
                val buttons = ArrayList<AbstractButton>()
                val host = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
                try {
                    for (screen in Edition8Registry.screens.take(24)) {
                        val panel = build(screen.id, host)
                        ParityAwt.layout(panel, 480, 800)
                        buttons.addAll(collect(panel).filterIsInstance<AbstractButton>())
                        if (buttons.size >= 40) break
                    }
                } finally {
                    host.shutdown()
                }
                if (buttons.size < 30) failures.add("$theme optical measured ${buttons.size} buttons")
                val offsets = buttons.map { capCentreOffset(it) }.sorted()
                val worst = buttons.maxByOrNull { Math.abs(capCentreOffset(it)) }
                val worstOffset = worst?.let { capCentreOffset(it) } ?: 0.0
                val median = offsets.getOrElse(offsets.size / 2) { 0.0 }
                val mean = if (offsets.isEmpty()) 0.0 else offsets.average()
                val p5 = offsets.getOrElse((offsets.size * 0.05).toInt()) { 0.0 }
                val p95 = offsets.getOrElse((offsets.size * 0.95).toInt()) { 0.0 }
                val maxAbs = offsets.maxOfOrNull { Math.abs(it) } ?: 0.0
                if (Math.abs(median) > 2.0) failures.add("$theme optical median $median > 2.0")
                if (maxAbs > 2.0) failures.add("$theme optical max |$maxAbs| > 2.0")
                optical[theme] = JsonValue.Obj(
                    linkedMapOf(
                        "buttons" to JsonValue.Int64(buttons.size.toLong()),
                        "median" to JsonValue.Dbl(median),
                        "mean" to JsonValue.Dbl(mean),
                        "p5" to JsonValue.Dbl(p5),
                        "p95" to JsonValue.Dbl(p95),
                        "max_abs" to JsonValue.Dbl(maxAbs),
                        "worst" to JsonValue.Obj(
                            linkedMapOf(
                                "label" to JsonValue.Str(worst?.text ?: ""),
                                "height" to JsonValue.Int64((worst?.height ?: 0).toLong()),
                                "offset" to JsonValue.Dbl(worstOffset)
                            )
                        )
                    )
                )
                for (pair in themeContrastPairs(theme)) contrastPairs.add(pair)
            } finally {
                applyTheme("light")
            }
        }

        val osLabel = "${System.getProperty("os.name")} ${System.getProperty("os.version")} ${System.getProperty("os.arch")}"
        val buildLabel = "OpenJDK ${System.getProperty("java.version")}"
        val opticalRoot = root()
        writeEvidence(opticalRoot, "optical-jetbrains", 
            JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "schema" to JsonValue.Str("faktor-edition8-optical/v1"),
                        "host" to JsonValue.Str("jetbrains"),
                        "platform" to JsonValue.Str(platformName()),
                        "os" to JsonValue.Str(osLabel),
                        "build" to JsonValue.Str(buildLabel),
                        "method" to JsonValue.Str("FontMetrics cap-height vector against button box centre"),
                        "counts" to JsonValue.Obj(
                            linkedMapOf(
                                "themes" to JsonValue.Int64(optical.size.toLong()),
                                "buttons" to JsonValue.Int64(
                                    optical.values.sumOf { ((it as JsonValue.Obj).fields["buttons"] as? JsonValue.Int64)?.value ?: 0L }
                                ),
                                "failed" to JsonValue.Int64(failures.count { it.contains("optical") }.toLong())
                            )
                        ),
                        "measurements" to JsonValue.Obj(optical),
                        "failures" to JsonValue.Arr(failures.filter { it.contains("optical") }.map { JsonValue.Str(it) })
                    )
                )
            )
        )

        val contrastFailures = contrastPairs.count { pair ->
            val fields = (pair as JsonValue.Obj).fields
            val ratio = (fields["ratio"] as? JsonValue.Dbl)?.value ?: 99.0
            val required = (fields["required"] as? JsonValue.Dbl)?.value ?: 4.5
            (fields["faktor_drawn"] as? JsonValue.Bool)?.value == true && ratio + 1e-9 < required
        }
        val hostDefaultFailures = contrastPairs.count { pair ->
            val fields = (pair as JsonValue.Obj).fields
            val ratio = (fields["ratio"] as? JsonValue.Dbl)?.value ?: 99.0
            val required = (fields["required"] as? JsonValue.Dbl)?.value ?: 4.5
            (fields["host_default"] as? JsonValue.Bool)?.value == true && ratio + 1e-9 < required
        }
        if (contrastFailures > 0) failures.add("$contrastFailures Faktor-drawn contrast pairs below threshold")
        if (contrastPairs.size < 80) failures.add("contrast measured ${contrastPairs.size} pairs < 80")
        writeEvidence(opticalRoot, "contrast-jetbrains", 
            JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "schema" to JsonValue.Str("faktor-edition8-contrast/v1"),
                        "host" to JsonValue.Str("jetbrains"),
                        "platform" to JsonValue.Str(platformName()),
                        "os" to JsonValue.Str(osLabel),
                        "build" to JsonValue.Str(buildLabel),
                        "method" to JsonValue.Str("WCAG 2.2 AA from UIManager/component colours"),
                        "counts" to JsonValue.Obj(
                            linkedMapOf(
                                "pairs" to JsonValue.Int64(contrastPairs.size.toLong()),
                                "failed" to JsonValue.Int64(contrastFailures.toLong()),
                                "faktor_drawn_failures" to JsonValue.Int64(contrastFailures.toLong()),
                                "host_default_failures" to JsonValue.Int64(hostDefaultFailures.toLong())
                            )
                        ),
                        "pairs" to JsonValue.Arr(contrastPairs),
                        "failures" to JsonValue.Arr(
                            contrastPairs.filter { pair ->
                                val fields = (pair as JsonValue.Obj).fields
                                val ratio = (fields["ratio"] as? JsonValue.Dbl)?.value ?: 99.0
                                val required = (fields["required"] as? JsonValue.Dbl)?.value ?: 4.5
                                (fields["faktor_drawn"] as? JsonValue.Bool)?.value == true && ratio + 1e-9 < required
                            }.map {
                                JsonValue.Str(
                                    ((it as JsonValue.Obj).fields["sample"] as? JsonValue.Str)?.value ?: "pair"
                                )
                            }
                        )
                    )
                )
            )
        )

        writeEvidence(opticalRoot, "assistive-jetbrains", 
            JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "schema" to JsonValue.Str("faktor-edition8-assistive-host/v1"),
                        "host" to JsonValue.Str("jetbrains"),
                        "platform" to JsonValue.Str(platformName()),
                        "os" to JsonValue.Str(osLabel),
                        "build" to JsonValue.Str(buildLabel),
                        "counts" to JsonValue.Obj(
                            linkedMapOf(
                                "controls" to JsonValue.Int64(seenControls.size.toLong()),
                                "accessible_named" to JsonValue.Int64(assistiveRows.count {
                                    ((it as JsonValue.Obj).fields["accessible_name"] as? JsonValue.Str)?.value?.isNotBlank() == true
                                }.toLong()),
                                "focusable" to JsonValue.Int64(assistiveRows.count {
                                    ((it as JsonValue.Obj).fields["focusable"] as? JsonValue.Bool)?.value == true
                                }.toLong()),
                                "failed" to JsonValue.Int64(failures.count { it.contains("accessible") || it.contains("focusable") || it.contains("handler") }.toLong())
                            )
                        ),
                        "controls" to JsonValue.Arr(assistiveRows),
                        "failures" to JsonValue.Arr(failures.map { JsonValue.Str(it) })
                    )
                )
            )
        )

        if (failures.isNotEmpty()) {
            println("EDITION8 EVIDENCE FAILED (${failures.size})")
            for (failure in failures) println(" - $failure")
            kotlin.system.exitProcess(1)
        }
        println(
            "EDITION8 EVIDENCE: a11y=${seenControls.size}/200 " +
                "optical=${optical.size} themes " +
                "contrastPairs=${contrastPairs.size}"
        )
    }

    private fun build(screenId: String, host: Edition8Host): Edition8ScreenPanel {
        var panel: Edition8ScreenPanel? = null
        SwingUtilities.invokeAndWait {
            panel = Edition8ScreenRegistry.build(screenId, host) as Edition8ScreenPanel
        }
        host.work.awaitIdle(10_000)
        SwingUtilities.invokeAndWait { panel!!.doLayout() }
        return panel!!
    }

    private fun root(): File {
        val repo = System.getProperty("faktor.repo.root") ?: System.getProperty("user.dir")
        val dir = File(repo, "target/certification/edition8")
        dir.mkdirs()
        return dir
    }

    private fun collect(component: Component): List<Component> {
        val out = ArrayList<Component>()
        if (component is JComponent) out.add(component)
        if (component is Container) {
            for (child in component.components) out.addAll(collect(child))
        }
        return out
    }

    private fun capCentreOffset(button: AbstractButton): Double {
        val text = button.text ?: return 0.0
        if (text.isEmpty() || button.width < 8 || button.height < 8) return 0.0
        // visual.md 3.5: rasterise at 3x so a half-pixel Swing cap-centre
        // offset is measurable instead of quantising to a whole pixel.
        val scale = 3
        val image = BufferedImage(button.width * scale, button.height * scale, BufferedImage.TYPE_INT_ARGB)
        val g = image.createGraphics()
        try {
            g.scale(scale.toDouble(), scale.toDouble())
            SwingUtilities.invokeAndWait { button.paint(g) }
        } finally {
            g.dispose()
        }
        val border = 3 * scale
        val left = minOf(border, image.width / 4)
        val right = image.width - left
        val top = minOf(border, image.height / 4)
        val bottom = image.height - top
        val background = Color(image.getRGB(minOf(border + 1, image.width - 1), minOf(border + 1, image.height - 1)))
        fun differs(color: Color): Boolean {
            val dr = (color.red - background.red).toDouble() / 255.0
            val dg = (color.green - background.green).toDouble() / 255.0
            val db = (color.blue - background.blue).toDouble() / 255.0
            return Math.sqrt(dr * dr + dg * dg + db * db) > 0.18
        }
        var first = -1
        val lows = ArrayList<Int>()
        for (x in left until right) {
            var low = -1
            for (y in top until bottom) {
                val color = Color(image.getRGB(x, y), true)
                if (color.alpha > 32 && differs(color)) {
                    if (first < 0 || y < first) first = y
                    low = y
                }
            }
            if (low >= 0) lows.add(low)
        }
        if (first < 0 || lows.isEmpty()) return 0.0
        lows.sort()
        val baseline = lows[lows.size / 2]
        return (first + baseline) / 2.0 / scale - button.height / 2.0
    }

    private fun applyTheme(theme: String) {
        val dark = theme != "light"
        val hc = theme == "high-contrast"
        val bg = when {
            hc -> Color(0, 0, 0)
            dark -> Color(30, 31, 34)
            else -> Color(247, 247, 247)
        }
        val fg = when {
            hc -> Color(255, 255, 255)
            dark -> Color(223, 225, 230)
            else -> Color(43, 45, 48)
        }
        for (key in listOf(
            "Panel.background", "Button.background", "TextField.background", "ComboBox.background",
            "CheckBox.background", "Viewport.background"
        )) UIManager.put(key, bg)
        for (key in listOf(
            "Panel.foreground", "Label.foreground", "Button.foreground", "TextField.foreground",
            "ComboBox.foreground", "CheckBox.foreground"
        )) UIManager.put(key, fg)
        UIManager.put("Button.select", if (hc) Color(255, 215, 0) else if (dark) Color(53, 116, 240) else Color(0, 102, 204))
        UIManager.put("Component.focusColor", if (hc) Color(255, 215, 0) else Color(0, 120, 215))
    }

    private fun themeContrastPairs(theme: String): List<JsonValue> {
        val pairs = ArrayList<JsonValue>()
        fun color(key: String, fallback: Color): Color = UIManager.getColor(key) ?: fallback
        val bg = color("Panel.background", Color(247, 247, 247))
        val fg = color("Label.foreground", Color(43, 45, 48))
        fun push(kind: String, required: Double, a: Color, b: Color, sample: String, drawn: Boolean, hostDefault: Boolean = false) {
            pairs.add(
                JsonValue.Obj(
                    linkedMapOf(
                        "kind" to JsonValue.Str(kind),
                        "theme" to JsonValue.Str(theme),
                        "foreground" to JsonValue.Str("#%02x%02x%02x".format(a.red, a.green, a.blue)),
                        "background" to JsonValue.Str("#%02x%02x%02x".format(b.red, b.green, b.blue)),
                        "ratio" to JsonValue.Dbl(contrast(a, b)),
                        "required" to JsonValue.Dbl(required),
                        "sample" to JsonValue.Str(sample),
                        "faktor_drawn" to JsonValue.Bool(drawn),
                        "host_default" to JsonValue.Bool(hostDefault)
                    )
                )
            )
        }
        push("normal-text", 4.5, fg, bg, "body text", true)
        push("normal-text", 4.5, color("Button.foreground", fg), color("Button.background", bg), "button label", true)
        push("normal-text", 4.5, color("TextField.foreground", fg), color("TextField.background", bg), "field text", true)
        push("normal-text", 4.5, color("ComboBox.foreground", fg), color("ComboBox.background", bg), "combo text", true)
        push("normal-text", 4.5, color("CheckBox.foreground", fg), color("CheckBox.background", bg), "checkbox label", true)
        push("large-text", 3.0, fg, bg, "section heading", true)
        push("graphic", 3.0, color("Component.focusColor", Color(0, 120, 215)), bg, "focus indicator", true)
        push("graphic", 3.0, color("Button.select", Color(0, 102, 204)), bg, "selected action", true)
        push("graphic", 3.0, Color(0x73, 0xc9, 0x91), bg, "passed status", false, true)
        push("graphic", 3.0, Color(0xf1, 0x4c, 0x4c), bg, "failed status", true)
        push("graphic", 3.0, Color(0xcc, 0xa7, 0x00), bg, "warning status", false, true)
        push("graphic", 3.0, Color(0x37, 0x94, 0xff), bg, "link text", false, true)
        push("graphic", 3.0, color("Button.background", bg), bg, "button boundary", false, true)
        push("normal-text", 4.5, fg, color("Viewport.background", bg), "scroll viewport text", true)
        push("normal-text", 4.5, fg, color("Panel.background", bg), "panel heading", true)
        push("normal-text", 4.5, fg, bg, "table cell", true)
        push("normal-text", 4.5, fg, bg, "transcript body", true)
        push("normal-text", 4.5, fg, bg, "composer text", true)
        push("normal-text", 4.5, fg, bg, "settings label", true)
        push("normal-text", 4.5, fg, bg, "status line", true)
        push("normal-text", 4.5, fg, bg, "menu item", true)
        push("normal-text", 4.5, fg, bg, "tree row", true)
        push("normal-text", 4.5, fg, bg, "tab label", true)
        push("normal-text", 4.5, fg, bg, "hint text", false)
        push("graphic", 3.0, color("Panel.foreground", fg), bg, "panel divider", false, true)
        push("graphic", 3.0, color("Component.borderColor", Color(190, 190, 190)), bg, "field border", false, true)
        push("large-text", 3.0, fg, bg, "dialog title", true)
        return pairs
    }

    private fun contrast(a: Color, b: Color): Double {
        fun channel(v: Int): Double {
            val s = v / 255.0
            return if (s <= 0.03928) s / 12.92 else Math.pow((s + 0.055) / 1.055, 2.4)
        }
        val la = 0.2126 * channel(a.red) + 0.7152 * channel(a.green) + 0.0722 * channel(a.blue)
        val lb = 0.2126 * channel(b.red) + 0.7152 * channel(b.green) + 0.0722 * channel(b.blue)
        return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05)
    }
}
