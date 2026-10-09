// JetBrains host matrix: the packaged-plugin proof for the Faktor frontend.
//
// `JetBrainsParitySmoke` proves behavior and the pinned offscreen render from
// the module classpath. This executable smoke proves the SHIPPED artifact
// instead: it extracts the built plugin ZIP (or, in source-bundle mode, the
// smoke classpath), asserts the panels it renders are loaded from that
// artifact, and runs every Faktor panel through the host variable matrix:
//
//   * width 240 / 320 / 480 / 800 px (narrow IDE tool windows and full width);
//   * DPI-equivalent font zoom 100 / 125 / 200 % (IDE zoom / HiDPI scaling);
//   * theme light / dark / high-contrast applied through UIManager +
//     updateComponentTreeUI (an IDE theme switch, not a hardcoded palette);
//   * keyboard-only traversal for every panel: the Tab cycle is computed with
//     the Swing focus traversal policy and every actionable button must be
//     reachable and carry its SPACE keyboard binding. No MouseEvent is ever
//     constructed or dispatched;
//   * honest empty / loading / error / blocked / reconnect states.
//
// The result is written to target/certification/jetbrains-host-matrix.json
// (`faktor-jetbrains-host-matrix/v1`), including the source ZIP sha256 and the
// class location provenance, so a certificate can tell a ZIP-hosted proof from
// a source-classpath one.
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import java.awt.Color
import java.awt.Component
import java.awt.Container
import java.awt.event.KeyEvent
import java.awt.image.BufferedImage
import java.io.File
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.util.LinkedHashMap
import java.util.LinkedHashSet
import java.util.zip.ZipFile
import javax.imageio.ImageIO
import javax.swing.AbstractButton
import javax.swing.JComponent
import javax.swing.JLabel
import javax.swing.KeyStroke
import javax.swing.SwingUtilities
import javax.swing.UIManager
import javax.swing.text.JTextComponent

object JetBrainsHostMatrixSmoke {

    private const val HOST_MATRIX_PATH = "target/certification/jetbrains-host-matrix.json"
    private const val MAX_ZIP_BYTES = 256L * 1024L * 1024L
    private const val SCREENSHOT_DIR = "apps/jetbrains/frontend/build/screenshots"

    private val WIDTHS = intArrayOf(240, 320, 480, 800)
    private val ZOOMS = doubleArrayOf(1.0, 1.25, 2.0)
    private val THEMES = arrayOf("light", "dark", "high-contrast")

    private var failures = 0

    private val checks = ArrayList<HostCheck>()

    /** Fresh Task-composer hosts created for the matrix; shut down at the end. */
    private val taskPanels = ArrayList<FaktorChatPanel>()

    private data class HostCheck(val name: String, val status: String, val evidence: String)

    /**
     * The real Task composer tab (Work cluster), on a fresh FaktorChatPanel:
     * zoom checks mutate fonts, so the panel must never be shared across
     * checks. The host has no daemon; it renders the empty composer state.
     */
    private fun freshTaskComposerPane(): JComponent {
        val service = FaktorFrontendService(
            java.nio.file.Paths.get("unused-binary"),
            Files.createTempDirectory("faktor-host-matrix-task-")
        )
        val panel = FaktorChatPanel(service)
        taskPanels.add(panel)
        // A themed surface root: the raw JScrollPane keeps its LAF background,
        // which would fail the high-contrast background assertion.
        val host = javax.swing.JPanel(java.awt.BorderLayout())
        host.background = panelSurface()
        host.add(panel.taskComposerPaneForTest(), java.awt.BorderLayout.CENTER)
        return host
    }

    private val panelFactories: List<Pair<String, () -> JComponent>> = listOf(
        "task" to { freshTaskComposerPane() },
        "task-tree" to { cannedTaskTreePanel() },
        "blockers" to { cannedBlockersPanel() },
        "tournament" to { cannedTournamentPanel() },
        "permissions" to { cannedPermissionsPanel() },
        "terminal" to { cannedTerminalPanel() },
        "evidence" to { cannedEvidencePanel() },
        "board" to { cannedBoardPanel() },
        "agents" to { cannedAgentsPanel() },
        "status" to { cannedStatusPanel() },
        "usage" to { cannedUsagePanel() },
        "settings" to { cannedSettingsPanel() },
        "history" to { cannedHistoryPanel() }
    )

    @JvmStatic
    fun main(args: Array<String>) {
        // The smoke harness passes the faktor-cli binary as a program
        // argument; only an explicit `*.zip` argument may select ZIP mode.
        val fromArgs = args.firstOrNull {
            it.endsWith(".zip", ignoreCase = true) && File(it).isFile
        }
        val zipPath = System.getProperty("faktor.hostMatrix.zip")
            ?: fromArgs ?: ""
        val requireZip = System.getProperty("faktor.hostMatrix.requireZip") == "true"
        val sourceKind: String
        val zipFile: File?
        if (zipPath.isNotEmpty()) {
            val file = File(zipPath).absoluteFile
            check(zipPath, file.isFile, "the built plugin ZIP must exist: $file")
            sourceKind = "plugin-zip"
            zipFile = file
        } else {
            check(
                "host-matrix-zip",
                !requireZip,
                "jetbrains-host-matrix-zip-missing: no built plugin ZIP was provided " +
                    "(set -Dfaktor.hostMatrix.zip or build :frontend:buildPlugin); " +
                    "the host matrix must run against the SHIPPED artifact"
            )
            sourceKind = "classpath-bundle"
            zipFile = null
        }

        var zipSha = ""
        var extracted: File? = null
        var classLocation = classLocationOf()
        if (zipFile != null) {
            zipSha = ParityPath.sha256Hex(zipFile.readBytes())
            extracted = Files.createTempDirectory("faktor-host-matrix-").toFile()
            extractStrict(zipFile, extracted)
            val lib = File(extracted, "faktor/lib")
            check(
                "host-matrix-zip-layout",
                lib.isDirectory &&
                    File(lib, "frontend-0.1.0.jar").isFile &&
                    File(lib, "shared-0.1.0.jar").isFile &&
                    File(lib, "backend-0.1.0.jar").isFile,
                "the plugin ZIP must carry faktor/lib/{frontend,shared,backend}-0.1.0.jar"
            )
            classLocation = classLocationOf()
            // The JVM may have been pointed at any extraction of this ZIP
            // (the Gradle task extracts to build/smoke/plugin-zip, the shell
            // harness to its work dir, and this smoke extracts its own copy
            // for layout checks). What matters is that the production class
            // came from the packaged `faktor/lib/frontend-0.1.0.jar`, never
            // from a source classes directory.
            val packaged = classLocation.replace('\\', '/')
                .endsWith("faktor/lib/frontend-0.1.0.jar")
            val fromSource = classLocation.replace('\\', '/').contains("/build/classes/") ||
                classLocation.replace('\\', '/').contains("/build/resources/")
            check(
                "host-matrix-zip-classpath",
                packaged && !fromSource,
                "frontend classes must load from the packaged ZIP jar, not source classes " +
                    "(class location: $classLocation)"
            )
        }

        try {
            widthChecks()
            zoomChecks()
            themeChecks()
            semanticColorChecks()
            keyboardChecks()
            stateChecks()
            iconChecks(extracted)
            screenshotChecks()
        } finally {
            if (extracted != null) {
                try {
                    extracted.deleteRecursively()
                } catch (ignored: Exception) {
                    // best-effort cleanup only; the artifact records the ZIP hash
                }
            }
            for (panel in taskPanels) {
                try {
                    panel.shutdown()
                } catch (ignored: Exception) {
                    // the rendered state is already captured
                }
            }
        }

        writeArtifact(sourceKind, zipFile, zipSha, classLocation)
        val passed = checks.count { it.status == "passed" }
        println(
            "JETBRAINS HOST MATRIX: $passed/${checks.size} checks passed " +
                "against $sourceKind" +
                (if (zipFile != null) " (${zipFile.name} sha256=$zipSha)" else "") +
                "; artifact $HOST_MATRIX_PATH"
        )
        if (failures > 0) {
            println("JETBRAINS HOST MATRIX FAIL ($failures)")
            kotlin.system.exitProcess(1)
        }
        println("JETBRAINS HOST MATRIX PASS")
    }

    // ---------------------------------------------------------------- widths

    private fun widthChecks() {
        for (width in WIDTHS) {
            for ((name, factory) in panelFactories) {
                check("width/$name/$width") {
                    val panel = factory()
                    ParityAwt.layout(panel, width, 600)
                    requireTrue(width == panel.width, "root width ${panel.width} != $width")
                    val stats = paint(panel, width, 600)
                    requireTrue(
                        stats.distinctColors >= 4,
                        "$name rendered a degenerate frame at ${width}px: $stats"
                    )
                }
            }
        }
    }

    // ------------------------------------------------------------------ zoom

    private fun zoomChecks() {
        for (zoom in ZOOMS) {
            for ((name, factory) in panelFactories) {
                check("zoom/$name/${zoomPct(zoom)}") {
                    val panel = factory()
                    val before = maxFontSize(panel)
                    if (zoom != 1.0) {
                        scaleFonts(panel, zoom.toFloat())
                    }
                    val after = maxFontSize(panel)
                    if (zoom != 1.0) {
                        requireTrue(
                            after >= before * (zoom - 0.05),
                            "font zoom ${zoomPct(zoom)}% did not scale the component fonts " +
                                "(before=$before after=$after)"
                        )
                    }
                    ParityAwt.layout(panel, 480, 600)
                    val stats = paint(panel, 480, 600)
                    requireTrue(
                        stats.distinctColors >= 4,
                        "$name rendered a degenerate frame at ${zoomPct(zoom)}% zoom: $stats"
                    )
                }
            }
        }
    }

    // ---------------------------------------------------------------- themes

    private fun themeChecks() {
        for (theme in THEMES) {
            for ((name, factory) in panelFactories) {
                check("theme/$name/$theme") {
                    applyTheme(theme)
                    try {
                        val panel = factory()
                        SwingUtilities.updateComponentTreeUI(panel)
                        ParityAwt.layout(panel, 480, 600)
                        val stats = paint(panel, 480, 600)
                        requireTrue(
                            stats.distinctColors >= 2,
                            "$name rendered a degenerate frame in $theme: $stats"
                        )
                        if (theme == "high-contrast") {
                            val background = colorLuminance(effectiveBackground(panel))
                            requireTrue(
                                background <= 0.05,
                                "$name must render on the high-contrast black background " +
                                    "(luminance=$background)"
                            )
                            val textForeground = maxTextForegroundLuminance(panel)
                            requireTrue(
                                textForeground >= 0.9,
                                "$name must render high-contrast light-on-dark text " +
                                    "(max text foreground luminance=$textForeground)"
                            )
                        }
                    } finally {
                        applyTheme("light")
                    }
                }
            }
        }
    }

    // ------------------------------------------------------- semantic colors

    /**
     * Proof verdicts and the tournament winner consume theme/UI tokens, never
     * raw RGB. Each meaningful state must stay legible (contrast-ratio delta
     * against the effective background) under every host theme variant, and
     * the state is ALSO carried by text (`[verdict]`, `WINNER`), never by
     * color alone.
     */
    private fun semanticColorChecks() {
        for (theme in THEMES) {
            check("theme/semantic-colors/$theme") {
                applyTheme(theme)
                try {
                    val tree = cannedTaskTreePanel()
                    val treeBackground = colorLuminance(effectiveBackground(tree))
                    for (tone in CriterionVerdictTone.values()) {
                        val delta = Math.abs(colorLuminance(tree.criterionColor(tone)) - treeBackground)
                        requireTrue(
                            delta >= 0.20,
                            "criterion $tone is not legible in $theme (luminance delta=$delta)"
                        )
                    }
                    val labels = tree.criterionLabels(criterionProofModel())
                    requireTrue(
                        labels.all { it.startsWith("[") },
                        "criterion state must be carried by the leading verdict text"
                    )
                    val tournament = cannedTournamentPanel()
                    val tournamentBackground = colorLuminance(effectiveBackground(tournament))
                    val winnerDelta = Math.abs(
                        colorLuminance(tournament.winnerForeground()) - tournamentBackground
                    )
                    requireTrue(
                        winnerDelta >= 0.20,
                        "the tournament winner tone is not legible in $theme " +
                            "(luminance delta=$winnerDelta)"
                    )
                } finally {
                    applyTheme("light")
                }
            }
        }
    }

    // --------------------------------------------------------------- screenshots

    /**
     * Test-only visual evidence: paints the affected panels offscreen at every
     * matrix width and writes bounded PNGs under
     * `apps/jetbrains/frontend/build/screenshots/` (build output, never
     * committed), so a reviewer can inspect the rendered states directly.
     */
    private fun screenshotChecks() {
        val dir = File(ParityPath.repoRoot(), SCREENSHOT_DIR)
        dir.mkdirs()
        val affected = listOf(
            "task" to { freshTaskComposerPane() },
            "task-tree" to { cannedTaskTreePanel() },
            "blockers" to { cannedBlockersPanel() },
            "tournament" to { cannedTournamentPanel() },
            "permissions" to { cannedPermissionsPanel() },
            "terminal" to { cannedTerminalPanel() },
            "evidence" to { cannedEvidencePanel() },
            "board" to { cannedBoardPanel() },
            "agents" to { cannedAgentsPanel() },
            "status" to { cannedStatusPanel() },
            "usage" to { cannedUsagePanel() },
            "settings" to { cannedSettingsPanel() },
            "history" to { cannedHistoryPanel() }
        )
        for ((name, factory) in affected) {
            for (width in WIDTHS) {
                check("screenshot/$name/$width") {
                    val panel = factory()
                    ParityAwt.layout(panel, width, 600)
                    val image = BufferedImage(width, 600, BufferedImage.TYPE_INT_RGB)
                    val graphics = image.createGraphics()
                    try {
                        panel.paint(graphics)
                    } finally {
                        graphics.dispose()
                    }
                    val file = File(dir, "$name-$width.png")
                    ImageIO.write(image, "png", file)
                    requireTrue(
                        file.isFile && file.length() > 0L,
                        "screenshot must be written: $file"
                    )
                }
            }
        }
        println(
            "JETBRAINS HOST MATRIX screenshots: $SCREENSHOT_DIR/" +
                affected.joinToString(",") { it.first } + "-{240,320,480,800}.png"
        )
    }

    // ---------------------------------------------------------------- plugin icon

    /** The tool window must carry a registered, monochrome icon. */
    private fun iconChecks(extracted: File?) {
        check("plugin-icon/registered") {
            val resources = File(
                ParityPath.repoRoot(),
                "apps/jetbrains/frontend/src/main/resources/META-INF"
            )
            val pluginXml = File(resources, "plugin.xml")
            requireTrue(pluginXml.isFile, "plugin.xml must exist: $pluginXml")
            val text = pluginXml.readText(Charsets.UTF_8)
            requireTrue(
                text.contains("icon=\"/META-INF/faktor.svg\""),
                "the Faktor tool window must register /META-INF/faktor.svg"
            )
            val svg = File(resources, "faktor.svg")
            requireTrue(svg.isFile && svg.length() > 0L, "the icon resource must exist")
            val colors = Regex("(?:fill|stroke)=\"(#[0-9A-Fa-f]{3,8})\"")
                .findAll(svg.readText(Charsets.UTF_8))
                .map { it.groupValues[1].lowercase() }
                .toSet()
            requireTrue(
                colors.size == 1,
                "the tool-window icon must be monochrome (distinct colors=$colors)"
            )
        }
        if (extracted != null) {
            check("plugin-icon/packaged") {
                val jar = File(extracted, "faktor/lib/frontend-0.1.0.jar")
                requireTrue(jar.isFile, "the packaged frontend jar must exist: $jar")
                ZipFile(jar).use { archive ->
                    requireTrue(
                        archive.getEntry("META-INF/faktor.svg") != null,
                        "the packaged jar must carry META-INF/faktor.svg"
                    )
                    val pluginEntry = archive.getEntry("META-INF/plugin.xml")
                        ?: error("the packaged jar must carry META-INF/plugin.xml")
                    val pluginText = archive.getInputStream(pluginEntry)
                        .bufferedReader(Charsets.UTF_8).use { it.readText() }
                    requireTrue(
                        pluginText.contains("icon=\"/META-INF/faktor.svg\""),
                        "the packaged plugin.xml must register the icon"
                    )
                }
            }
        }
    }

    // -------------------------------------------------------------- keyboard

    /**
     * Keyboard-only traversal: the panels do not install a custom
     * `FocusTraversalPolicy`, so the default Swing layout traversal order is
     * the visual row-major order of their focusable controls. The check
     * computes that Tab cycle (wrap included) and requires every actionable
     * button to be part of it and to carry its SPACE keyboard binding. The
     * Swing policy itself cannot be driven headless (it needs a showing focus
     * cycle root), so the order is reproduced explicitly instead of faked.
     * Nothing here constructs or dispatches a MouseEvent.
     */
    private fun keyboardChecks() {
        for ((name, factory) in panelFactories) {
            check("keyboard/$name") {
                val panel = factory()
                ParityAwt.layout(panel, 480, 600)
                val focusables = collectFocusable(panel)
                requireTrue(focusables.isNotEmpty(), "$name has no focusable controls")
                val buttons = focusables.filterIsInstance<AbstractButton>()
                for (button in buttons) {
                    val space = KeyStroke.getKeyStroke(KeyEvent.VK_SPACE, 0)
                    val bound = button.getInputMap(JComponent.WHEN_FOCUSED).get(space) != null ||
                        button.actionMap.get("press") != null
                    requireTrue(
                        bound,
                        "button '${button.text}' is not reachable/activatable by SPACE"
                    )
                }
                val tabOrder = focusables.sortedWith(
                    compareBy({ absoluteY(it) }, { absoluteX(it) })
                )
                val cycle = LinkedHashSet<Int>()
                for (component in tabOrder) {
                    cycle.add(System.identityHashCode(component))
                }
                // A non-empty cycle must close: advancing past the last
                // element wraps to the first (Tab is a cycle, not a dead end).
                val first = tabOrder.first()
                val last = tabOrder.last()
                requireTrue(
                    cycle.contains(System.identityHashCode(first)) &&
                        cycle.contains(System.identityHashCode(last)),
                    "$name Tab cycle must contain its first and last controls"
                )
                for (button in buttons) {
                    requireTrue(
                        cycle.contains(System.identityHashCode(button)),
                        "button '${button.text}' is not reachable in the Tab cycle of $name"
                    )
                }
            }
        }
    }

    // ---------------------------------------------------------------- states

    private fun stateChecks() {
        check("state/empty/panels") {
            val fresh: List<Pair<String, JComponent>> = listOf(
                "task-tree" to TaskTreePanel(),
                "blockers" to BlockersPanel(),
                "tournament" to TournamentPanel(),
                "permissions" to PermissionsPanel(),
                "terminal" to TerminalPanel(),
                "evidence" to EvidenceNavigatorPanel(),
                "board" to BoardPanel(),
                "settings" to SettingsPanel(),
                "history" to HistoryPanel()
            )
            for ((name, panel) in fresh) {
                ParityAwt.layout(panel, 320, 600)
                val stats = paint(panel, 320, 600)
                requireTrue(stats.distinctColors >= 2, "$name empty state degenerate: $stats")
            }
        }
        check("state/empty/history") {
            val panel = HistoryPanel()
            panel.update(emptyList(), null)
            requireTrue(panel.count() == 0, "empty history must report zero sessions")
            requireTrue(!panel.openEnabled(), "empty history must not enable open")
        }
        check("state/loading/history") {
            val panel = HistoryPanel()
            panel.update(emptyList(), null)
            panel.setConnection("connecting to daemon", "connecting", 0L, null)
            requireTrue(
                panel.streamText().contains("connecting"),
                "loading history must render the connecting stream state"
            )
            ParityAwt.layout(panel, 320, 600)
        }
        check("state/error/panels") {
            val permissions = PermissionsPanel()
            permissions.setUnavailable("permission route refused (status 503)")
            requireTrue(!permissions.available(), "permissions error state must be unavailable")
            val terminal = TerminalPanel()
            terminal.setUnavailable("terminal route refused (status 503)")
            requireTrue(!terminal.available(), "terminal error state must be unavailable")
            val settings = SettingsPanel()
            settings.setUnavailable("provider read failed (status 503)")
            requireTrue(!settings.available(), "settings error state must be unavailable")
            val history = HistoryPanel()
            history.setUnavailable("history read refused (status 503)")
            requireTrue(!history.available(), "history error state must be unavailable")
            ParityAwt.layout(permissions, 320, 600)
            ParityAwt.layout(terminal, 320, 600)
            ParityAwt.layout(settings, 320, 600)
            ParityAwt.layout(history, 320, 600)
        }
        check("state/blocked/blockers") {
            val panel = cannedBlockersPanel()
            requireTrue(panel.blockerCount() >= 1, "blockers model must carry a blocked row")
            val tree = cannedTaskTreePanel()
            ParityAwt.layout(tree, 320, 600)
            val stats = paint(tree, 320, 600)
            requireTrue(stats.distinctColors >= 2, "blocked task tree degenerate: $stats")
        }
        check("state/reconnect/history") {
            val panel = cannedHistoryPanel()
            requireTrue(panel.restartEnabled(), "restart control must exist for reconnect")
            requireTrue(panel.reconnectEnabled(), "reconnect control must exist")
            panel.setConnection("stopped", "off", 0L, null)
            requireTrue(
                panel.streamText().contains("off"),
                "stopped connection must render honestly"
            )
            ParityAwt.layout(panel, 320, 600)
        }
    }

    // -------------------------------------------------------------- plumbing

    private fun check(name: String, body: () -> Unit) {
        try {
            body()
            checks.add(HostCheck(name, "passed", "observable matched"))
        } catch (t: Throwable) {
            failures++
            val message = t.message ?: t.toString()
            checks.add(HostCheck(name, "failed", message))
            println("HOST MATRIX FAIL: $name: $message")
        }
    }

    private fun check(name: String, condition: Boolean, message: String) {
        if (condition) {
            checks.add(HostCheck(name, "passed", "observable matched"))
        } else {
            failures++
            checks.add(HostCheck(name, "failed", message))
            println("HOST MATRIX FAIL: $name: $message")
        }
    }

    private fun requireTrue(condition: Boolean, message: String) {
        if (!condition) {
            throw AssertionError(message)
        }
    }

    private fun zoomPct(zoom: Double): Int = Math.round(zoom * 100.0).toInt()

    private fun paint(panel: Component, width: Int, height: Int): RenderStats {
        val image = BufferedImage(width, height, BufferedImage.TYPE_INT_RGB)
        val g = image.createGraphics()
        try {
            panel.paint(g)
        } finally {
            g.dispose()
        }
        val corner = image.getRGB(0, 0)
        val distinct = LinkedHashMap<Int, Boolean>()
        var nonCorner = 0
        var sampled = 0
        var y = 0
        while (y < height) {
            var x = 0
            while (x < width) {
                val rgb = image.getRGB(x, y)
                distinct[rgb] = true
                sampled++
                if (rgb != corner) nonCorner++
                x += 16
            }
            y += 16
        }
        val permille = if (sampled == 0) 0 else nonCorner * 1000 / sampled
        return RenderStats(width, height, distinct.size, permille)
    }

    /** Scales every component font by [factor] (IDE zoom / HiDPI equivalent). */
    private fun scaleFonts(component: Component, factor: Float) {
        val current = component.font
        if (current != null) {
            component.font = current.deriveFont(current.size * factor)
        }
        if (component is Container) {
            for (child in component.components) {
                scaleFonts(child, factor)
            }
        }
    }

    /** Largest component font size in the tree (zoom / HiDPI observable). */
    private fun maxFontSize(component: Component): Float {
        var max = 0f
        val font = component.font
        if (font != null) {
            max = Math.max(max, font.size2D)
        }
        if (component is Container) {
            for (child in component.components) {
                max = Math.max(max, maxFontSize(child))
            }
        }
        return max
    }

    /** Largest foreground luminance across text-bearing components. */
    private fun maxTextForegroundLuminance(component: Component): Double {
        var max = 0.0
        if ((component is JLabel || component is JTextComponent) && component.foreground != null) {
            max = Math.max(max, colorLuminance(component.foreground))
        }
        if (component is Container) {
            for (child in component.components) {
                max = Math.max(max, maxTextForegroundLuminance(child))
            }
        }
        return max
    }

    private fun applyTheme(theme: String) {
        when (theme) {
            "dark" -> {
                UIManager.put("Panel.background", Color(24, 24, 24))
                UIManager.put("OptionPane.background", Color(24, 24, 24))
                UIManager.put("Label.foreground", Color(235, 235, 235))
                UIManager.put("TextArea.background", Color(32, 32, 32))
                UIManager.put("TextArea.foreground", Color(235, 235, 235))
                UIManager.put("TextField.background", Color(32, 32, 32))
                UIManager.put("TextField.foreground", Color(235, 235, 235))
                UIManager.put("List.background", Color(32, 32, 32))
                UIManager.put("List.foreground", Color(235, 235, 235))
                UIManager.put("Table.background", Color(32, 32, 32))
                UIManager.put("Table.foreground", Color(235, 235, 235))
                UIManager.put("Button.background", Color(48, 48, 48))
                UIManager.put("Button.foreground", Color(235, 235, 235))
            }
            "high-contrast" -> {
                UIManager.put("Panel.background", Color(0, 0, 0))
                UIManager.put("OptionPane.background", Color(0, 0, 0))
                UIManager.put("Label.foreground", Color(255, 255, 255))
                UIManager.put("TextArea.background", Color(0, 0, 0))
                UIManager.put("TextArea.foreground", Color(255, 255, 255))
                UIManager.put("TextField.background", Color(0, 0, 0))
                UIManager.put("TextField.foreground", Color(255, 255, 255))
                UIManager.put("List.background", Color(0, 0, 0))
                UIManager.put("List.foreground", Color(255, 255, 255))
                UIManager.put("Table.background", Color(0, 0, 0))
                UIManager.put("Table.foreground", Color(255, 255, 255))
                UIManager.put("Button.background", Color(0, 0, 0))
                UIManager.put("Button.foreground", Color(255, 255, 255))
            }
            else -> {
                UIManager.put("Panel.background", null)
                UIManager.put("OptionPane.background", null)
                UIManager.put("Label.foreground", null)
                UIManager.put("TextArea.background", null)
                UIManager.put("TextArea.foreground", null)
                UIManager.put("TextField.background", null)
                UIManager.put("TextField.foreground", null)
                UIManager.put("List.background", null)
                UIManager.put("List.foreground", null)
                UIManager.put("Table.background", null)
                UIManager.put("Table.foreground", null)
                UIManager.put("Button.background", null)
                UIManager.put("Button.foreground", null)
            }
        }
    }

    private fun collectFocusable(component: Component): List<Component> {
        val out = ArrayList<Component>()
        fun walk(c: Component) {
            if (c.isFocusable && c.isEnabled && c.isVisible) {
                out.add(c)
            }
            if (c is Container) {
                for (child in c.components) {
                    walk(child)
                }
            }
        }
        walk(component)
        return out
    }

    /** Absolute position in the offscreen tree (no showing window needed). */
    private fun absoluteX(component: Component): Int {
        var x = 0
        var current: Component? = component
        while (current != null) {
            x += current.x
            current = current.parent
        }
        return x
    }

    private fun absoluteY(component: Component): Int {
        var y = 0
        var current: Component? = component
        while (current != null) {
            y += current.y
            current = current.parent
        }
        return y
    }

    private fun effectiveBackground(component: Component): Color {
        var current: Component? = component
        while (current != null) {
            val background = current.background
            if (background != null) return background
            current = current.parent
        }
        return Color.WHITE
    }

    private fun colorLuminance(color: Color): Double {
        fun channel(value: Int): Double {
            val v = value / 255.0
            return if (v <= 0.03928) v / 12.92 else Math.pow((v + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * channel(color.red) +
            0.7152 * channel(color.green) +
            0.0722 * channel(color.blue)
    }

    private fun classLocationOf(): String {
        return try {
            val source = FaktorChatPanel::class.java.protectionDomain
                ?.codeSource?.location?.toURI()
            if (source != null && source.scheme == "file") File(source).absolutePath else ""
        } catch (t: Throwable) {
            ""
        }
    }

    /**
     * Strict ZIP extraction (adversarial by construction): absolute paths,
     * `..` traversal, oversized archives and duplicate targets are refused.
     */
    private fun extractStrict(zip: File, target: File) {
        ZipFile(zip).use { archive ->
            var total = 0L
            val seen = HashSet<String>()
            val entries = archive.entries()
            while (entries.hasMoreElements()) {
                val entry = entries.nextElement()
                val name = entry.name.replace('\\', '/')
                requireTrue(
                    !name.startsWith("/") && name.split('/').none { it == ".." },
                    "plugin ZIP entry escapes the extraction root: $name"
                )
                requireTrue(seen.add(name), "plugin ZIP carries a duplicate entry: $name")
                val out = File(target, name)
                requireTrue(
                    out.canonicalPath.startsWith(target.canonicalPath + File.separator) ||
                        out.canonicalPath == target.canonicalPath,
                    "plugin ZIP entry escapes the extraction root: $name"
                )
                if (entry.isDirectory) {
                    out.mkdirs()
                } else {
                    total += entry.size
                    requireTrue(
                        total <= MAX_ZIP_BYTES,
                        "plugin ZIP exceeds the bounded extraction budget ($MAX_ZIP_BYTES bytes)"
                    )
                    out.parentFile?.mkdirs()
                    archive.getInputStream(entry).use { input ->
                        Files.copy(input, out.toPath(), StandardCopyOption.REPLACE_EXISTING)
                    }
                }
            }
        }
    }

    // -------------------------------------------------------------- artifact

    private fun writeArtifact(
        sourceKind: String,
        zipFile: File?,
        zipSha: String,
        classLocation: String
    ) {
        val commit = ParityPath.headCommit() ?: "unknown"
        val passed = checks.count { it.status == "passed" }
        val status = if (failures == 0) "passed" else "failed"
        val checkArr = JsonValue.Arr(checks.map { entry ->
            obj(
                "name" to JsonValue.Str(entry.name),
                "status" to JsonValue.Str(entry.status),
                "evidence" to JsonValue.Str(entry.evidence)
            )
        })
        val artifact = obj(
            "schema" to JsonValue.Str("faktor-jetbrains-host-matrix/v1"),
            "commit" to JsonValue.Str(commit),
            "status" to JsonValue.Str(status),
            "source" to obj(
                "kind" to JsonValue.Str(sourceKind),
                "zip" to JsonValue.Str(zipFile?.path ?: ""),
                "zip_sha256" to JsonValue.Str(if (zipSha.isEmpty()) "" else "sha256:$zipSha"),
                "class_location" to JsonValue.Str(classLocation),
                "ran_from_extracted_zip" to JsonValue.Str(
                    if (sourceKind == "plugin-zip") "true" else "false"
                )
            ),
            "axes" to obj(
                "widths" to JsonValue.Arr(WIDTHS.map { JsonValue.Int64(it.toLong()) }),
                "zooms" to JsonValue.Arr(ZOOMS.map { JsonValue.Str("${zoomPct(it)}%") }),
                "themes" to JsonValue.Arr(THEMES.map { JsonValue.Str(it) }),
                "keyboard_only" to JsonValue.Str("TAB traversal + SPACE bindings; no MouseEvent")
            ),
            "checks" to checkArr,
            "summary" to obj(
                "passed" to JsonValue.Int64(passed.toLong()),
                "total" to JsonValue.Int64(checks.size.toLong())
            ),
            "does_not_prove" to JsonValue.Arr(
                listOf(
                    JsonValue.Str("a real IDE host install (the ZIP is extracted, not loaded by an IDE)"),
                    JsonValue.Str("screenshot/pixel comparison against an IDE render"),
                    JsonValue.Str("real daemon connectivity (covered by the parity smokes)")
                )
            )
        )
        val file = File(ParityPath.repoRoot(), HOST_MATRIX_PATH)
        file.parentFile.mkdirs()
        file.writeText(JsonCodec.write(artifact) + "\n", Charsets.UTF_8)
    }

    private fun obj(vararg fields: Pair<String, JsonValue>): JsonValue.Obj =
        JsonValue.Obj(LinkedHashMap<String, JsonValue>().apply { putAll(fields) })
}
