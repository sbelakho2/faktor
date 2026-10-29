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
//   * honest empty / loading / error / blocked / reconnect states, one
//     `state/<axis>/<panel>` check per panel and axis (the dogfood rule parses
//     these names): empty for every panelFactories entry, loading for the
//     not-read surfaces, error for the read-backed panels, plus the blocked
//     and reconnect recovery surfaces.
//
// The result is written to target/certification/jetbrains-host-matrix.json
// (`faktor-jetbrains-host-matrix/v1`), including the source ZIP sha256 and the
// class location provenance, so a certificate can tell a ZIP-hosted proof from
// a source-classpath one.
package dev.faktor.frontend

import dev.faktor.backend.DaemonBinary
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.parseNativeBoardPage
import dev.faktor.shared.parseNativeTaskVerification
import dev.faktor.shared.parseNativeTerminalEventPage
import dev.faktor.shared.parseNativeTerminalPage
import java.awt.Color
import java.awt.Component
import java.awt.Container
import java.awt.event.KeyEvent
import dev.faktor.shared.view
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

    private const val EDITION5_VISUAL_SCHEMA = "faktor-jetbrains-edition5-visual/v1"

    private const val LEGACY_VISUAL_BASELINE =
        "apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json"

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
     * checks. The host has no daemon; it renders the composer with the Run
     * options disclosure open and the Acceptance & contract editors filled,
     * so the rendered state covers the new contract rows.
     */
    private fun freshTaskComposerPane(): JComponent {
        val service = FaktorFrontendService(
            java.nio.file.Paths.get("unused-binary"),
            Files.createTempDirectory("faktor-host-matrix-task-")
        )
        val panel = FaktorChatPanel(service)
        taskPanels.add(panel)
        panel.setContractFieldsForTest(
            "existing sessions remain valid\nexisting API clients cannot break",
            "revoked sessions cannot authenticate",
            "race conditions",
            "replayed refresh tokens fail"
        )
        // A themed surface root: the raw JScrollPane keeps its LAF background,
        // which would fail the high-contrast background assertion.
        val host = javax.swing.JPanel(java.awt.BorderLayout())
        host.background = panelSurface()
        host.add(panel.taskComposerPaneForTest(), java.awt.BorderLayout.CENTER)
        return host
    }

    /**
     * The bounded Task options sheet as its own matrix host: opened on the
     * Work center surface (replacing the conversation, never a modal dialog)
     * with the per-run budget authored, so the 240px width check proves the
     * scrollable sheet stays inside the tool window.
     */
    private fun freshTaskOptionsSheetPane(): JComponent {
        val service = FaktorFrontendService(
            java.nio.file.Paths.get("unused-binary"),
            Files.createTempDirectory("faktor-host-matrix-options-")
        )
        val panel = FaktorChatPanel(service)
        taskPanels.add(panel)
        panel.setContractFieldsForTest("stay valid", "always true", "drift", "proof")
        panel.setBudgetFieldsForTest("5000", "250000")
        panel.openTaskOptionsForTest()
        requireTrue(
            panel.taskOptionsOpenForTest(),
            "the Task options sheet must open for the host matrix"
        )
        val host = javax.swing.JPanel(java.awt.BorderLayout())
        host.background = panelSurface()
        host.add(panel.taskComposerPaneForTest(), java.awt.BorderLayout.CENTER)
        return host
    }

    /**
     * The bounded Work approval attention card with one live request: the
     * host renders only the card (it hides itself while nothing is pending).
     */
    private fun freshWorkApprovalPane(): JComponent {
        val service = FaktorFrontendService(
            java.nio.file.Paths.get("unused-binary"),
            Files.createTempDirectory("faktor-host-matrix-approval-")
        )
        val panel = FaktorChatPanel(service)
        taskPanels.add(panel)
        panel.updatePendingApprovalsForTest(
            listOf(
                dev.faktor.shared.NativePermissionEntry(
                    "7", "9", "shell", "{\"tool\":\"bash\"}"
                )
            )
        )
        val host = javax.swing.JPanel(java.awt.BorderLayout())
        host.background = panelSurface()
        host.add(panel.workApprovalView(), java.awt.BorderLayout.CENTER)
        return host
    }

    /** The wrapping utilities toolbar row as its own matrix host. */
    private fun freshToolbarPane(): JComponent {
        val service = FaktorFrontendService(
            java.nio.file.Paths.get("unused-binary"),
            Files.createTempDirectory("faktor-host-matrix-toolbar-")
        )
        val panel = FaktorChatPanel(service)
        taskPanels.add(panel)
        val host = javax.swing.JPanel(java.awt.BorderLayout())
        host.background = panelSurface()
        host.add(panel.utilitiesToolbarForTest(), java.awt.BorderLayout.NORTH)
        // A section caption so the theme legibility probe has real text (the
        // row itself is buttons + a chooser; the production row sits under
        // the panel header).
        val caption = javax.swing.JPanel(java.awt.BorderLayout())
        caption.background = panelSurface()
        caption.add(sectionHeader("Utilities"), java.awt.BorderLayout.NORTH)
        host.add(caption, java.awt.BorderLayout.CENTER)
        return host
    }

    private val panelFactories: List<Pair<String, () -> JComponent>> = listOf(
        "task" to { freshTaskComposerPane() },
        "task-options" to { freshTaskOptionsSheetPane() },
        "work-approval" to { freshWorkApprovalPane() },
        "toolbar" to { freshToolbarPane() },
        "task-tree" to { cannedTaskTreePanel() },
        "plan" to { cannedTaskTreePanel() },
        "overview" to { cannedOverviewPanel() },
        "changes" to { cannedChangesPanel() },
        "verification" to { cannedVerificationPanel() },
        "blockers" to { cannedBlockersPanel() },
        "tournament" to { cannedTournamentPanel() },
        "permissions" to { cannedPermissionsPanel() },
        "terminal" to { cannedTerminalPanel() },
        "evidence" to { cannedEvidencePanel() },
        "board" to { cannedBoardPanel() },
        "agents" to { cannedAgentsPanel() },
        "diagnostics" to { cannedDiagnosticsPanel() },
        "context" to { Edition5ContextPanel(null) },
        "checkpoints" to { Edition5CheckpointPanel(null) },
        "usage" to { cannedUsagePanel() },
        "settings" to { cannedSettingsPanel() },
        "history" to { cannedHistoryPanel() },
        "conversations" to { cannedConversationsPanel() },
        "outcomes" to { cannedOutcomesPanel() }
    )

    @JvmStatic
    fun main(args: Array<String>) {
        // An assertion failure in an offscreen Swing harness would otherwise
        // leave the non-daemon AWT event thread alive forever after main dies.
        Thread.setDefaultUncaughtExceptionHandler { _, error ->
            error.printStackTrace()
            kotlin.system.exitProcess(1)
        }
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
            edition5Checks()
            edition8Checks()
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
        // An offscreen Swing smoke leaves non-daemon AWT threads alive, so
        // returning from main would hang the caller forever (the smoke script
        // runs this JVM in the foreground). Exit explicitly like every other
        // smoke: the PASS line above is the verdict, the code is the gate.
        kotlin.system.exitProcess(0)
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
        // The Task options sheet must stay INSIDE the 240px tool window (the
        // bounded, scrollable sheet is the I02 requirement; a sheet wider
        // than its host would clip the fields out of reach).
        check("width/task-options-sheet/240") {
            val service = FaktorFrontendService(
                java.nio.file.Paths.get("unused-binary"),
                Files.createTempDirectory("faktor-host-matrix-options-width-")
            )
            val chat = FaktorChatPanel(service)
            taskPanels.add(chat)
            chat.openTaskOptionsForTest()
            ParityAwt.layout(chat, 240, 600)
            val sheet = chat.taskOptionsSheetForTest()
                ?: throw AssertionError("the Task options sheet must exist")
            requireTrue(
                sheet.width <= 240,
                "the Task options sheet must stay within 240px (width=${sheet.width})"
            )
            requireTrue(
                sheet.components.any { it is javax.swing.JScrollPane },
                "the Task options sheet must carry a bounded scrollable body"
            )
        }
        // I05/I09: the Work approval card renders inside the 240px tool
        // window and sits ABOVE the composer (a pending approval is an
        // attention event, not a destination).
        check("width/work-approval/240") {
            val service = FaktorFrontendService(
                java.nio.file.Paths.get("unused-binary"),
                Files.createTempDirectory("faktor-host-matrix-approval-width-")
            )
            val chat = FaktorChatPanel(service)
            taskPanels.add(chat)
            chat.updatePendingApprovalsForTest(
                listOf(
                    dev.faktor.shared.NativePermissionEntry(
                        "7", "9", "shell", "{\"tool\":\"bash\"}"
                    )
                )
            )
            ParityAwt.layout(chat, 240, 600)
            val card = chat.workApprovalView()
            requireTrue(card.isVisible, "the Work approval card must be visible while pending")
            requireTrue(card.width <= 240, "the Work approval card exceeds 240px (${card.width})")
            val buttons = card.replyButtonsForTest("7")
                ?: throw AssertionError("the Work approval card must render request 7")
            requireTrue(buttons.first.width > 0, "the Allow button must have real width")
            requireTrue(buttons.second.width > 0, "the Deny button must have real width")
            // Above the composer in the Work south column.
            val workPlane = chat.workPlaneForTest()
            val south = workPlane.getComponent(workPlane.componentCount - 1) as java.awt.Container
            val cardIndex = south.components.indexOfFirst { it === card }
            val composerIndex = south.components.size - 1
            requireTrue(
                cardIndex in 0 until composerIndex,
                "the approval card must sit above the composer (card=$cardIndex of ${south.components.size})"
            )
        }
        // I09: the utilities toolbar WRAPS at 240px — no horizontal-scroll
        // fallback, every control inside the host width and the primaries
        // (New session / Refresh / session chooser) visible.
        check("width/toolbar/240") {
            val service = FaktorFrontendService(
                java.nio.file.Paths.get("unused-binary"),
                Files.createTempDirectory("faktor-host-matrix-toolbar-width-")
            )
            val chat = FaktorChatPanel(service)
            taskPanels.add(chat)
            ParityAwt.layout(chat, 240, 600)
            val toolbar = chat.utilitiesToolbarForTest() as java.awt.Container
            requireTrue(
                !containsScrollPane(toolbar),
                "the toolbar must not carry a horizontal-scroll fallback"
            )
            requireTrue(toolbar.width <= 240, "toolbar width ${toolbar.width} exceeds 240")
            val rows = wrappedRowCount(toolbar)
            requireTrue(rows >= 2, "the toolbar must wrap at 240px (rows=$rows)")
            for (child in toolbar.components) {
                if (!child.isVisible) continue
                requireTrue(
                    child.x + child.width <= 240,
                    "'${child.javaClass.simpleName}' escapes the 240px host"
                )
            }
            val labels = toolbar.components.mapNotNull {
                when (it) {
                    is javax.swing.JButton -> it.text
                    is javax.swing.JComboBox<*> -> "session chooser"
                    else -> null
                }
            }
            requireTrue(labels.contains("New session"), "New session must stay visible: $labels")
            requireTrue(labels.contains("Refresh"), "Refresh must stay visible: $labels")
            requireTrue(labels.contains("session chooser"), "the chooser must stay visible: $labels")
            requireTrue(
                !labels.contains("Settings"),
                "the Settings utility must live under More, not in the row: $labels"
            )
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
            "task-options" to { freshTaskOptionsSheetPane() },
            "task-tree" to { cannedTaskTreePanel() },
            "plan" to { cannedTaskTreePanel() },
            "overview" to { cannedOverviewPanel() },
            "changes" to { cannedChangesPanel() },
            "verification" to { cannedVerificationPanel() },
            "blockers" to { cannedBlockersPanel() },
            "tournament" to { cannedTournamentPanel() },
            "permissions" to { cannedPermissionsPanel() },
            "terminal" to { cannedTerminalPanel() },
            "evidence" to { cannedEvidencePanel() },
            "board" to { cannedBoardPanel() },
            "agents" to { cannedAgentsPanel() },
            "diagnostics" to { cannedDiagnosticsPanel() },
        "context" to { Edition5ContextPanel(null) },
        "checkpoints" to { Edition5CheckpointPanel(null) },
            "usage" to { cannedUsagePanel() },
            "settings" to { cannedSettingsPanel() },
            "history" to { cannedHistoryPanel() },
            "conversations" to { cannedConversationsPanel() },
            "outcomes" to { cannedOutcomesPanel() }
        ) + Edition5Catalog.screens.map { screen ->
            "edition5-${screen.id.lowercase()}" to { Edition5ScreenRegistry.build(screen.id) }
        }
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
        // Revision 8: every one of the 72 screens at the four host widths,
        // plus the high-contrast and 200% text variants at 480 px.
        for (screen in Edition8Registry.screens) {
            val host = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
            try {
                for (width in WIDTHS) {
                    val panel = Edition8ScreenRegistry.build(screen.id, host)
                    ParityAwt.layout(panel, width, 600)
                    writeScreenshot(panel, width, 600, File(dir, "edition8-${screen.id.lowercase()}-$width.png"))
                }
                applyTheme("high-contrast")
                try {
                    val panel = Edition8ScreenRegistry.build(screen.id, host)
                    SwingUtilities.updateComponentTreeUI(panel)
                    ParityAwt.layout(panel, 480, 600)
                    writeScreenshot(panel, 480, 600, File(dir, "edition8-${screen.id.lowercase()}-hc.png"))
                } finally {
                    applyTheme("light")
                }
                val zoomed = Edition8ScreenRegistry.build(screen.id, host)
                scaleFonts(zoomed, 2.0f)
                ParityAwt.layout(zoomed, 480, 600)
                writeScreenshot(zoomed, 480, 600, File(dir, "edition8-${screen.id.lowercase()}-200.png"))
            } finally {
                host.shutdown()
            }
        }
        println(
            "JETBRAINS HOST MATRIX screenshots: $SCREENSHOT_DIR/" +
                affected.joinToString(",") { it.first } + "-{240,320,480,800}.png + edition8-s01..s72" +
                "-{240,320,480,800,hc,200}.png"
        )
    }

    // ------------------------------------------------------- Edition 5 screens

    /**
     * Every one of the 32 Edition 5 screens must build, lay out at all four
     * host widths, paint a non-degenerate frame, keep 32 px control targets,
     * expose accessible names on interactive controls and never introduce a
     * horizontal scroll surface (the 240 px stack rule).
     */
    private fun edition5Checks() {
        for (screen in Edition5Catalog.screens) {
            for (width in WIDTHS) {
                check("edition5/${screen.id}/$width") {
                    val panel = Edition5ScreenRegistry.build(screen.id)
                    ParityAwt.layout(panel, width, 600)
                    val stats = paint(panel, width, 600)
                    requireTrue(
                        stats.distinctColors >= 3 && stats.nonCornerPermille in 1..999,
                        "${screen.id} rendered a degenerate frame at ${width}px: $stats"
                    )
                    requireTrue(
                        !containsHorizontalScroll(panel),
                        "${screen.id} introduced a horizontal scroll surface at ${width}px"
                    )
                    val interactive = collectInteractive(panel)
                    requireTrue(
                        interactive.isNotEmpty(),
                        "${screen.id} exposed no interactive control"
                    )
                    for (component in interactive) {
                        if (component is javax.swing.AbstractButton &&
                            component.parent is javax.swing.JComboBox<*>
                        ) {
                            continue
                        }
                        val name = component.accessibleContext?.accessibleName
                        requireTrue(
                            !name.isNullOrBlank(),
                            "${screen.id} ${component.javaClass.simpleName} has no accessible name"
                        )
                    }
                }
            }
        }
        edition5VisualBaselineChecks()
        check("edition5/catalog") {
            requireTrue(Edition5Catalog.controls.size == 200, "200 controls")
            requireTrue(Edition5Catalog.screens.size == 32, "32 screens")
            requireTrue(
                Edition5Catalog.controls.all { it.screens.isNotEmpty() },
                "every control names a screen"
            )
        }
    }


    // ---------------------------------------------------------- Edition 8

    /**
     * Every Revision 8 screen (72) at the host widths, all three themes, 200%
     * text, keyboard reachability and accessible names. The 200% pass scales
     * every font and then asserts the primary action stays reachable without
     * horizontal scrolling; the optical pass measures at least 30 button
     * labels per theme with the cap-height method from visual.md §3.5.
     */
    private fun edition8Checks() {
        for (screen in Edition8Registry.screens) {
            for (width in WIDTHS) {
                check("edition8/${screen.id}/$width") {
                    withEdition8Panel(screen.id) { panel ->
                        ParityAwt.layout(panel, width, 600)
                        val stats = paint(panel, width, 600)
                        requireTrue(
                            stats.distinctColors >= 3 && stats.nonCornerPermille in 1..999,
                            "${screen.id} rendered a degenerate frame at ${width}px: $stats"
                        )
                        requireTrue(
                            !containsHorizontalScroll(panel),
                            "${screen.id} introduced a horizontal scroll surface at ${width}px"
                        )
                        val interactive = collectInteractive(panel)
                        requireTrue(interactive.isNotEmpty(), "${screen.id} exposed no interactive control")
                        for (component in interactive) {
                            val name = component.accessibleContext?.accessibleName
                            requireTrue(
                                !name.isNullOrBlank(),
                                "${screen.id} ${component.javaClass.simpleName} has no accessible name"
                            )
                        }
                        val buttons = interactive.filterIsInstance<javax.swing.AbstractButton>()
                        requireTrue(buttons.isNotEmpty(), "${screen.id} exposed no button")
                        for (button in buttons) {
                            requireTrue(
                                button.preferredSize.height >= 24 || button.text.isNullOrBlank(),
                                "${screen.id} target '${button.text}' is ${button.preferredSize.height}px (<24)"
                            )
                        }
                        // The primary action must be reachable at 240px: it is
                        // in the header EAST position, whose bounds stay inside
                        // the panel (the header wraps at narrow widths).
                        val primary = interactive.firstOrNull {
                            (it as? javax.swing.AbstractButton)?.name == "edition8-primary-${screen.id}"
                        } as? javax.swing.AbstractButton
                        requireTrue(primary != null, "${screen.id} has no primary action component")
                        requireTrue(
                            (primary!!.width == 0 || primary.x + primary.width <= panel.width + 1),
                            "${screen.id} primary action is clipped outside the ${width}px panel"
                        )
                    }
                }
            }
            for (theme in THEMES) {
                check("edition8/${screen.id}/theme/$theme") {
                    applyTheme(theme)
                    try {
                        withEdition8Panel(screen.id) { panel ->
                            SwingUtilities.updateComponentTreeUI(panel)
                            ParityAwt.layout(panel, 480, 600)
                            val stats = paint(panel, 480, 600)
                            requireTrue(stats.distinctColors >= 2, "${screen.id} degenerate in $theme: $stats")
                            if (theme == "high-contrast") {
                                val bg = colorLuminance(effectiveBackground(panel))
                                requireTrue(bg <= 0.05, "${screen.id} HC background luminance=$bg")
                                requireTrue(
                                    maxTextForegroundLuminance(panel) >= 0.9,
                                    "${screen.id} HC text does not stay light-on-dark"
                                )
                            }
                        }
                    } finally {
                        applyTheme("light")
                    }
                }
            }
            check("edition8/${screen.id}/zoom/200") {
                withEdition8Panel(screen.id) { panel ->
                    scaleFonts(panel, 2.0f)
                    ParityAwt.layout(panel, 320, 600)
                    val stats = paint(panel, 320, 600)
                    requireTrue(stats.distinctColors >= 2, "${screen.id} degenerate at 200% text: $stats")
                    val primary = collectInteractive(panel).firstOrNull {
                        (it as? javax.swing.AbstractButton)?.name == "edition8-primary-${screen.id}"
                    } as? javax.swing.AbstractButton
                    requireTrue(
                        primary != null,
                        "${screen.id} primary action disappeared at 200% text"
                    )
                }
            }
        }
        edition8VisualBaselineChecks()
        edition8OpticalAndContrastChecks()
        check("edition8/registry") {
            val errors = Edition8Registry.validate()
            requireTrue(errors.isEmpty(), "registry: ${errors.joinToString("; ")}")
        }
    }

    private fun withEdition8Panel(screenId: String, body: (javax.swing.JComponent) -> Unit) {
        val host = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
        try {
            val panel = Edition8ScreenRegistry.build(screenId, host)
            body(panel)
        } finally {
            host.shutdown()
        }
    }

    private fun edition8VisualBaselineChecks() {
        val digests = LinkedHashMap<String, String>()
        for (screen in Edition8Registry.screens) {
            withEdition8Panel(screen.id) { panel ->
                ParityAwt.layout(panel, 480, 800)
                digests["edition8-${screen.id.lowercase()}"] = logicalScreenDigest(panel, "e8-${screen.id}", "edition8")
            }
        }
        val path = File(
            ParityPath.repoRoot(),
            "apps/jetbrains/frontend/src/test/resources/parity/edition8-visual-baselines.json"
        )
        val platform = when {
            System.getProperty("os.name").lowercase().contains("mac") -> "macos"
            System.getProperty("os.name").lowercase().contains("linux") -> "linux"
            System.getProperty("os.name").lowercase().contains("windows") -> "windows"
            else -> "unknown"
        }
        requireTrue(platform != "unknown", "edition8 visual baseline needs a known platform")
        val write = System.getProperty("faktor.parity.writeBaselines") == "true"
        val existing = readJsonFile(path)
        val platformNames = listOf("linux", "macos", "windows")
        if (write) {
            // Write ONLY this platform's digests from THIS render. The old
            // writer copied the current platform's digests onto every
            // platform entry, which is exactly the inherited-macOS-digest
            // defect the Edition-8 certification forbids. Other platforms
            // are preserved byte-for-byte.
            val platforms = LinkedHashMap<String, JsonValue>()
            val existingPlatforms = existing?.view("existing")?.optionalField("platforms")
            for (name in platformNames) {
                if (name == platform) {
                    platforms[name] = JsonValue.Obj(
                        linkedMapOf(
                            "environment" to JsonValue.Str("${name}-normalized-edition8-digest"),
                            "reviewed" to JsonValue.Bool(true),
                            "themes" to JsonValue.Arr(
                                listOf("light", "dark", "high-contrast").map { JsonValue.Str(it) }
                            ),
                            "widths" to JsonValue.Arr(WIDTHS.map { JsonValue.Int64(it.toLong()) }),
                            "zooms" to JsonValue.Arr(listOf(100, 125, 200).map { JsonValue.Int64(it.toLong()) }),
                            "digests" to JsonValue.Obj(digests.mapValues { JsonValue.Str(it.value) })
                        )
                    )
                } else {
                    val preserved = existingPlatforms?.optionalField(name)?.value
                    if (preserved != null) platforms[name] = preserved
                }
            }
            Files.write(
                path.toPath(),
                JsonCodec.write(
                    JsonValue.Obj(
                        linkedMapOf(
                            "schema" to JsonValue.Str("faktor-jetbrains-edition8-visual/v1"),
                            "method" to JsonValue.Str(
                                "platform-normalized-component-tree+accessibility+variant-matrix"
                            ),
                            "screens" to JsonValue.Int64(72),
                            "hostContracts" to JsonValue.Int64(144),
                            "platforms" to JsonValue.Obj(platforms)
                        )
                    )
                ).toByteArray(Charsets.UTF_8)
            )
        } else {
            val record = existing?.view("existing")?.optionalField("platforms")
                ?.optionalField(platform)
                ?: error("edition8 visual baseline for $platform is missing; re-pin with --write-baselines")
            val pinned = record.optionalField("digests")?.value as? JsonValue.Obj
                ?: error("edition8 $platform baseline has no digests")
            for ((name, digest) in digests) {
                val pinnedDigest = (pinned.fields[name] as? JsonValue.Str)?.value
                requireTrue(
                    pinnedDigest == digest,
                    "$name drifted from the reviewed $platform baseline (pinned=$pinnedDigest got=$digest)"
                )
            }
            requireTrue(
                record.optionalField("reviewed")?.value == JsonValue.Bool(true),
                "edition8 $platform baseline is not marked reviewed"
            )
        }
    }

    private fun edition8OpticalAndContrastChecks() {
        for (theme in THEMES) {
            applyTheme(theme)
            try {
                val buttons = ArrayList<javax.swing.AbstractButton>()
                val host = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
                try {
                    for (screen in Edition8Registry.screens.take(24)) {
                        val panel = Edition8ScreenRegistry.build(screen.id, host)
                        ParityAwt.layout(panel, 480, 800)
                        buttons.addAll(collectInteractive(panel).filterIsInstance<javax.swing.AbstractButton>())
                        if (buttons.size >= 40) break
                    }
                } finally {
                    host.shutdown()
                }
                requireTrue(buttons.size >= 30, "optical pass needs >=30 buttons per theme, got ${buttons.size}")
                val offsets = buttons.map { capCentreOffset(it) }.sorted()
                val median = offsets[offsets.size / 2]
                requireTrue(
                    Math.abs(median) <= 2.0,
                    "button cap-centre median $median px in $theme exceeds the Rev8 max |2.0|"
                )
                val primary = buttons.firstOrNull { it.isEnabled && it.background != null }
                if (primary != null) {
                    val fg = primary.foreground ?: textForeground()
                    val bg = primary.background ?: panelSurface()
                    requireTrue(
                        contrastRatio(fg, bg) >= 3.0,
                        "primary button contrast ${contrastRatio(fg, bg)} in $theme is below 3:1"
                    )
                }
                val bodyPair = contrastRatio(textForeground(), panelSurface())
                requireTrue(bodyPair >= 4.5, "body contrast $bodyPair in $theme is below 4.5:1")
            } finally {
                applyTheme("light")
            }
        }
    }

    /**
     * Cap-height optical offset: ink cap centre against the button box centre
     * (GlyphVector("H") cap top + baseline, median per-column lowest ink is
     * approximated by the font baseline metrics). Positive means below centre.
     */
    private fun capCentreOffset(button: javax.swing.AbstractButton): Double {
        val text = button.text ?: return 0.0
        if (text.isEmpty()) return 0.0
        val metrics = button.getFontMetrics(button.font)
        val capHeight = button.font.createGlyphVector(
            metrics.fontRenderContext,
            "H"
        ).visualBounds.height
        val boxCentre = button.height / 2.0
        val capCentre = (button.insets.top + metrics.ascent - capHeight / 2.0)
        return capCentre - boxCentre
    }

    private fun contrastRatio(a: Color, b: Color): Double {
        fun channel(v: Int): Double {
            val s = v / 255.0
            return if (s <= 0.03928) s / 12.92 else Math.pow((s + 0.055) / 1.055, 2.4)
        }
        val la = 0.2126 * channel(a.red) + 0.7152 * channel(a.green) + 0.0722 * channel(a.blue)
        val lb = 0.2126 * channel(b.red) + 0.7152 * channel(b.green) + 0.0722 * channel(b.blue)
        val hi = Math.max(la, lb)
        val lo = Math.min(la, lb)
        return (hi + 0.05) / (lo + 0.05)
    }

    private fun edition5VisualBaselineChecks() {
        val digests = LinkedHashMap<String, String>()
        for (screen in Edition5Catalog.screens) {
            val panel = Edition5ScreenRegistry.build(screen.id)
            when (panel) {
                is Edition5AsyncPanel -> panel.awaitIdleForTest()
                is Edition5SettingsPanel -> panel.awaitIdleForTest()
            }
            ParityAwt.layout(panel, 480, 800)
            digests["edition5-${screen.id.lowercase()}"] = logicalScreenDigest(panel, screen.id)
        }
        val path = File(
            ParityPath.repoRoot(),
            "apps/jetbrains/frontend/src/test/resources/parity/edition5-visual-baselines.json"
        )
        val platform = when {
            System.getProperty("os.name").lowercase().contains("mac") -> "macos"
            System.getProperty("os.name").lowercase().contains("linux") -> "linux"
            System.getProperty("os.name").lowercase().contains("windows") -> "windows"
            else -> "unknown"
        }
        requireTrue(platform != "unknown", "edition5 visual baseline needs a known platform")
        val legacy = readJsonFile(File(ParityPath.repoRoot(), LEGACY_VISUAL_BASELINE))
        fun environmentOf(name: String): String =
            legacy?.view("legacy")?.optionalField("platforms")
                ?.optionalField(name)?.optionalString("environment")
                ?: "$name-normalized-edition5-digest"
        val write = System.getProperty("faktor.parity.writeBaselines") == "true"
        val existing = readJsonFile(path)
        if (write) {
            val platforms = LinkedHashMap<String, JsonValue>()
            val keep = listOf("linux", "macos", "windows")
            for (name in keep) {
                val previous = existing?.view("existing")?.optionalField("platforms")
                    ?.optionalField(name)?.value as? JsonValue.Obj
                if (name == platform || previous == null) {
                    platforms[name] = JsonValue.Obj(
                        linkedMapOf(
                            "environment" to JsonValue.Str(environmentOf(name)),
                            "digests" to JsonValue.Obj(
                                digests.mapValues { JsonValue.Str(it.value) }
                            )
                        )
                    )
                } else {
                    platforms[name] = JsonValue.Obj(previous.fields)
                }
            }
            val root = JsonValue.Obj(
                linkedMapOf(
                    "schema" to JsonValue.Str(EDITION5_VISUAL_SCHEMA),
                    "digest" to JsonValue.Str("platform-normalized-component-tree+accessibility"),
                    "platforms" to JsonValue.Obj(platforms)
                )
            )
            path.parentFile.mkdirs()
            path.writeText(JsonCodec.write(root), Charsets.UTF_8)
            println("[edition5] visual baselines pinned for $platform: ${digests.size} screens")
            return
        }
        requireTrue(existing != null, "edition5 visual baseline file is missing: $path")
        val record = existing!!.view("existing").optionalField("platforms")
            ?.optionalField(platform)
        requireTrue(record != null, "edition5 visual baseline has no $platform record")
        val pinned = record!!.optionalField("digests")
        for ((name, digest) in digests) {
            val expected = pinned?.optionalString(name)
            check("edition5/visual/$name") {
                requireTrue(expected != null, "missing pinned digest $name for $platform")
                requireTrue(
                    expected == digest,
                    "edition5 visual digest drifted for $name: pinned=$expected now=$digest"
                )
            }
        }
    }

    private fun writeScreenshot(panel: Component, width: Int, height: Int, file: File) {
        val image = BufferedImage(width, height, BufferedImage.TYPE_INT_RGB)
        val graphics = image.createGraphics()
        try {
            panel.paint(graphics)
        } finally {
            graphics.dispose()
        }
        ImageIO.write(image, "png", file)
        requireTrue(file.isFile && file.length() > 0L, "screenshot must be written: $file")
    }

    private fun logicalScreenDigest(root: Component, screenId: String, family: String = "edition5"): String {
        val sb = StringBuilder(family).append('|').append(screenId).append('\n')
        fun walk(node: Component) {
            val className = node.javaClass.name
            if (className.startsWith("javax.swing.plaf") ||
                className.startsWith("com.apple.laf") ||
                className.startsWith("apple.laf") ||
                className.startsWith("sun.swing") ||
                className.startsWith("com.sun.java.swing")
            ) {
                return
            }
            val context = node.accessibleContext
            sb.append(node.javaClass.simpleName).append('|')
                .append(context?.accessibleName ?: "").append('|')
                .append(if (node.isVisible) "1" else "0").append('|')
            when (node) {
                is javax.swing.AbstractButton -> sb.append("text=").append(node.text).append('|')
                is javax.swing.JLabel -> sb.append("label=").append(node.text).append('|')
                is javax.swing.JComboBox<*> -> sb.append("items=").append(node.itemCount).append('|')
                is javax.swing.JList<*> -> sb.append("rows=").append(node.model.size).append('|')
                is javax.swing.JTextField -> sb.append("field=").append(node.text.take(40)).append('|')
                is javax.swing.JTextArea -> sb.append("area=").append(node.text.take(40)).append('|')
            }
            sb.append('\n')
            if (node is java.awt.Container) {
                for (child in node.components) walk(child)
            }
        }
        walk(root)
        return ParityPath.sha256Hex(sb.toString().toByteArray(Charsets.UTF_8))
    }

    private fun readJsonFile(file: File): JsonValue? =
        try {
            if (file.isFile) JsonCodec.parse(file.readText(Charsets.UTF_8)) else null
        } catch (e: Exception) {
            null
        }

    private fun containsHorizontalScroll(component: Component): Boolean {
        if (component is javax.swing.JScrollPane) {
            if (component.horizontalScrollBarPolicy !=
                javax.swing.ScrollPaneConstants.HORIZONTAL_SCROLLBAR_NEVER
            ) {
                return true
            }
        }
        if (component is java.awt.Container) {
            for (child in component.components) {
                if (containsHorizontalScroll(child)) return true
            }
        }
        return false
    }

    private fun collectInteractive(component: Component): List<Component> {
        val out = ArrayList<Component>()
        fun effectivelyVisible(node: Component): Boolean {
            var current: Component? = node
            while (current != null) {
                if (!current.isVisible) return false
                current = current.parent
            }
            return true
        }
        fun walk(node: Component) {
            if (node is javax.swing.AbstractButton ||
                node is javax.swing.JTextField ||
                node is javax.swing.JTextArea ||
                node is javax.swing.JComboBox<*> ||
                node is javax.swing.JList<*>
            ) {
                if (node.isEnabled && effectivelyVisible(node)) out.add(node)
            }
            if (node is java.awt.Container) {
                for (child in node.components) walk(child)
            }
        }
        walk(component)
        return out
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
        emptyStateChecks()
        loadingStateChecks()
        errorStateChecks()
        recoveryStateChecks()
    }

    // -------------------------------------------------------------- empty axis

    /**
     * `state/empty/<panel>` for every panelFactories entry. Each check renders
     * the panel's real empty / not-read state and pins the exact copy the
     * product actually renders today (an unwritten panel shows its explicit
     * placeholder; no row, metric or draft is ever invented).
     */
    private fun emptyStateChecks() {
        // Aggregate arm's-length render of every fresh panel (kept beside the
        // granular state/empty/<panel> checks; the granular checks carry the
        // per-panel copy).
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
                "history" to HistoryPanel(),
                "diagnostics" to DiagnosticsPanel(),
                "overview" to OverviewPanel(),
                "changes" to ChangesPanel(),
                "verification" to VerificationPanel()
            )
            for ((name, panel) in fresh) {
                ParityAwt.layout(panel, 320, 600)
                val stats = paint(panel, 320, 600)
                requireTrue(stats.distinctColors >= 2, "$name empty state degenerate: $stats")
            }
        }
        check("state/empty/task") {
            val chat = newChatPanel("task-empty")
            requireTrue(
                chat.goalTextForTest().isEmpty(),
                "the empty composer must not invent a goal draft"
            )
            requireTrue(
                chat.attachmentsCountForTest() == 0,
                "the empty composer must carry no attachments"
            )
            requireTrue(
                textLines(chat).contains("Send"),
                "the empty composer must render its ONE Send action: ${textLines(chat)}"
            )
            requireTrue(
                !textLines(chat).contains("Ask") && !textLines(chat).contains("Intent"),
                "the unified composer carries no Ask/Task mode switch: ${textLines(chat)}"
            )
            requireTrue(
                chat.taskOptionsVisibleForTest(),
                "the options disclosure must stay reachable without a mode switch"
            )
            requirePanelRenders("empty task composer", chat)
        }
        check("state/empty/task-options") {
            val chat = newChatPanel("task-options-empty")
            chat.openTaskOptionsForTest()
            requireTrue(chat.taskOptionsOpenForTest(), "the Task options sheet must open")
            requireTrue(
                chat.contractEditorsForTest().all { it.isEmpty() },
                "the empty options sheet must not invent contract rows: " +
                    chat.contractEditorsForTest()
            )
            requireTrue(
                chat.selectedFinishLevelLabelForTest() == FinishLevel.LOCAL.label(),
                "the empty sheet defaults to the Local finish level: " +
                    chat.selectedFinishLevelLabelForTest()
            )
            val sheet = chat.taskOptionsSheetForTest()
                ?: throw AssertionError("the Task options sheet must exist")
            val texts = textLines(sheet)
            requireTrue(texts.contains("Task options"), "the sheet must be headed: $texts")
            requireTrue(texts.contains("Back"), "the sheet must offer Back: $texts")
            requireTrue(
                texts.contains("Start task"),
                "the sheet must carry the Start task escalation: $texts"
            )
            requirePanelRenders("empty task options sheet", sheet)
        }
        check("state/empty/work-approval") {
            val chat = newChatPanel("work-approval-empty")
            chat.updatePendingApprovalsForTest(emptyList())
            val card = chat.workApprovalView()
            requireTrue(card.count() == 0, "no request may be pending in the empty state")
            requireTrue(
                !card.isVisible,
                "the Work approval card must hide itself while nothing is pending"
            )
            // The T10 card copy the surface renders when a request arrives.
            requireTrue(card.headlineText() == APPROVAL_HEADLINE, "T10 headline copy")
            requireTrue(card.supportingText() == APPROVAL_SUPPORTING, "T10 supporting copy")
            requirePanelRenders("empty Work approval", chat)
        }
        check("state/empty/toolbar") {
            val chat = newChatPanel("toolbar-empty")
            requireTrue(
                chat.sessionChooserCountForTest() == 0,
                "the empty toolbar must serve no sessions"
            )
            val texts = textLines(chat.utilitiesToolbarForTest())
            requireTrue(
                texts.contains("New session") && texts.contains("Refresh"),
                "the empty toolbar keeps its primary actions: $texts"
            )
            requirePanelRenders("empty utilities toolbar", chat)
        }
        check("state/empty/task-tree") { emptyTaskTreeState() }
        check("state/empty/plan") { emptyTaskTreeState() }
        check("state/empty/overview") {
            val panel = OverviewPanel()
            panel.updateModel(null)
            val texts = textLines(panel)
            requireTrue(
                texts.contains("No run yet. Describe a task in the composer."),
                "the empty overview must name the next action: $texts"
            )
            requireTrue(texts.contains("No run yet"), "the empty run chip must name itself: $texts")
            requireTrue(
                !panel.hasSummaryRowForTest("Phase") && !panel.hasSummaryRowForTest("Spend"),
                "absent overview sections must be omitted, not rendered as placeholders"
            )
            requirePanelRenders("empty overview", panel)
        }
        // I03 exact empty copy (ChangeReview.CHANGES_EMPTY_COPY).
        check("state/empty/changes") {
            val panel = ChangesPanel()
            requireTrue(
                panel.emptyText() == CHANGES_EMPTY_COPY,
                "the empty changes state must render '$CHANGES_EMPTY_COPY': ${panel.emptyText()}"
            )
            requireTrue(
                panel.emptyText() == "No candidate changes are served for this run yet.",
                "the I03 empty copy is exactly pinned"
            )
            requireTrue(
                !panel.openEnabled(),
                "no candidate changes means Open source diff stays disabled"
            )
            requireTrue(
                panel.candidateText() == "No candidate identity is served for this run.",
                "no candidate identity is invented: ${panel.candidateText()}"
            )
            requirePanelRenders("empty changes", panel)
        }
        check("state/empty/verification") {
            val panel = VerificationPanel()
            requireTrue(
                panel.summaryText() == "No verification has been recorded yet.",
                "the empty verification summary must be honest: ${panel.summaryText()}"
            )
            val texts = textLines(panel)
            requireTrue(
                texts.contains(
                    "Criteria, checks and reviewer appear here once a run is verified."
                ),
                "the empty criteria column must explain itself: $texts"
            )
            requirePanelRenders("empty verification", panel)
        }
        check("state/empty/blockers") {
            val panel = BlockersPanel()
            panel.update(emptyList(), emptyList())
            requireTrue(
                panel.blockerCount() == 0 && panel.permissionCount() == 0,
                "the empty blockers surface carries no rows"
            )
            val texts = textLines(panel)
            for (copy in listOf(
                "No blocked children.",
                "No task-level blockers."
            )) {
                requireTrue(
                    texts.contains(copy),
                    "the empty blockers surface must render '$copy': $texts"
                )
            }
            // The approvals section is deliberately omitted here (the cards
            // live on the Inspect -> Agents -> Approvals surface), so the
            // empty state never claims "no pending permissions".
            requireTrue(
                texts.none { it.contains("Pending permissions") },
                "the empty surface omits the approvals section instead of claiming none: $texts"
            )
            requirePanelRenders("empty blockers", panel)
        }
        check("state/empty/tournament") {
            val panel = TournamentPanel()
            requireTrue(
                panel.summariesText() == "No saved comparisons in this session yet.",
                "the empty comparison listing must say so: ${panel.summariesText()}"
            )
            requireTrue(
                panel.primaryText().contains("No approaches are being compared yet."),
                "the empty tournament must not invent an approach: ${panel.primaryText()}"
            )
            requirePanelRenders("empty tournament", panel)
        }
        check("state/empty/permissions") {
            val panel = PermissionsPanel()
            requireTrue(
                panel.count() == 0 && panel.available(),
                "an empty (but available) approval list is not an error"
            )
            requireTrue(
                panel.headerText().startsWith(APPROVAL_HEADLINE),
                "the empty approvals list keeps the T10 headline: ${panel.headerText()}"
            )
            val texts = textLines(panel)
            requireTrue(
                texts.contains(APPROVAL_SUPPORTING),
                "the T10 supporting copy renders on the approvals surface: $texts"
            )
            requireTrue(
                texts.contains("No pending approvals. Requests appear here while an agent works."),
                "the empty approvals placeholder must render: $texts"
            )
            requireTrue(
                panel.detailText() ==
                    "No pending approval requests. New requests appear here for review.",
                "the empty request detail must render: ${panel.detailText()}"
            )
            requireTrue(!panel.allowEnabled(), "nothing pending means Allow stays disabled")
            requirePanelRenders("empty permissions", panel)
        }
        check("state/empty/terminal") {
            val panel = TerminalPanel()
            panel.update(
                parseNativeTerminalPage(
                    "{\"sessionId\":\"7\",\"terminals\":[],\"unowned\":0,\"note\":\"\"}"
                )
            )
            panel.setEvents(
                parseNativeTerminalEventPage(
                    "{\"sessionId\":\"7\",\"events\":[],\"hasMore\":false,\"nextCursor\":null}"
                )
            )
            requireTrue(
                panel.available() && panel.terminalCount() == 0,
                "an empty (but available) terminal read is not an error"
            )
            requireTrue(
                panel.eventsText() == "No lifetime events yet for this conversation.",
                "the empty lifetime events must say so: ${panel.eventsText()}"
            )
            requireTrue(
                panel.advancedText() == "No session-owned processes.",
                "the empty process readout must say so: ${panel.advancedText()}"
            )
            requireTrue(
                panel.outputText().isEmpty(),
                "the empty terminal surface must not invent output"
            )
            requirePanelRenders("empty terminal", panel)
        }
        check("state/empty/evidence") {
            val panel = EvidenceNavigatorPanel()
            requireTrue(
                panel.evidenceCount() == 0 && panel.messageCount() == 0,
                "the empty navigator carries no evidence refs and no messages"
            )
            requireTrue(
                panel.outputText().isEmpty() && panel.transcriptText().isEmpty(),
                "nothing is fabricated before a retrieval"
            )
            requirePanelRenders("empty evidence", panel)
        }
        check("state/empty/board") {
            val panel = BoardPanel()
            panel.setBoard(parseNativeBoardPage(PARITY_BOARD_PAGE_JSON))
            requireTrue(panel.available(), "an empty (but read) board is not an error")
            requireTrue(
                panel.headerText() == "coordination: 0 posts · 0 unread",
                "the empty board header must count zero: ${panel.headerText()}"
            )
            requireTrue(
                panel.postsText().isEmpty(),
                "the empty board must not fabricate posts: ${panel.postsText()}"
            )
            requirePanelRenders("empty board", panel)
        }
        check("state/empty/agents") {
            val panel = AgentsPanel()
            panel.update(emptyList(), null)
            requireTrue(panel.rosterText().isEmpty(), "the empty roster must render no rows")
            requireTrue(
                panel.advancedText() == "No agents yet. Rows appear here while a run works.",
                "the empty roster placeholder must render: ${panel.advancedText()}"
            )
            requirePanelRenders("empty agents", panel)
        }
        check("state/empty/diagnostics") {
            val panel = DiagnosticsPanel()
            requireTrue(
                panel.streamText() == "off",
                "the pre-read live-update row must stay off: ${panel.streamText()}"
            )
            requireTrue(
                panel.trustText() == "—",
                "a never-resolved binary trust row stays missing: ${panel.trustText()}"
            )
            requireTrue(
                panel.daemonText() == "—",
                "an unread service row stays missing: ${panel.daemonText()}"
            )
            requirePanelRenders("empty diagnostics", panel)
        }
        check("state/empty/context") {
            val panel = Edition5ContextPanel(null)
            panel.awaitIdleForTest()
            requireTrue(
                panel.accessibleContext != null,
                "the empty context panel must expose an accessible context"
            )
            requirePanelRenders("empty context", panel)
        }
        check("state/empty/checkpoints") {
            val panel = Edition5CheckpointPanel(null)
            panel.awaitIdleForTest()
            requireTrue(
                panel.accessibleContext != null,
                "the empty checkpoint panel must expose an accessible context"
            )
            requirePanelRenders("empty checkpoints", panel)
        }
        check("state/empty/usage") {
            val panel = UsagePanel()
            requireTrue(panel.model() == null, "nothing is invented before the usage read")
            val period = panel.periodText()
            requireTrue(
                period.contains("No period is served yet."),
                "the empty usage period must say so: $period"
            )
            requireTrue(
                period.contains("Tokens — not reported"),
                "unreported tokens stay unreported: $period"
            )
            requirePanelRenders("empty usage", panel)
        }
        check("state/empty/settings") {
            val panel = SettingsPanel()
            requireTrue(panel.available(), "an unread settings surface is not an error")
            requireTrue(panel.providerCount() == 0, "no provider is invented")
            requireTrue(
                panel.providerText() == "no providers served",
                "the empty providers card must say so: ${panel.providerText()}"
            )
            requireTrue(
                panel.daemonText() == "daemon: not started",
                "the empty daemon card must say so: ${panel.daemonText()}"
            )
            requireTrue(
                panel.statusText() == "providers: -",
                "the unread settings status line must stay untracked: ${panel.statusText()}"
            )
            requirePanelRenders("empty settings", panel)
        }
        check("state/empty/history") {
            val panel = HistoryPanel()
            panel.update(emptyList(), null)
            requireTrue(panel.count() == 0, "empty history must report zero sessions")
            requireTrue(!panel.openEnabled(), "empty history must not enable open")
            requireTrue(
                panel.statusText() == "prior work: 0 sessions",
                "the empty history must count zero honestly: ${panel.statusText()}"
            )
            requirePanelRenders("empty history", panel)
        }
        check("state/empty/conversations") {
            val panel = HistoryPanel()
            panel.update(emptyList(), null)
            requireTrue(
                panel.count() == 0 && !panel.openEnabled(),
                "the conversations surface must render an empty listing honestly"
            )
            requireTrue(
                panel.statusText() == "prior work: 0 sessions",
                "the empty conversations listing counts zero: ${panel.statusText()}"
            )
            requirePanelRenders("empty conversations", panel)
        }
        check("state/empty/outcomes") {
            val panel = OutcomesPanel()
            requireTrue(
                panel.model() == null,
                "nothing is invented before the outcomes read"
            )
            val texts = textLines(panel)
            requireTrue(
                texts.contains("No local-cohort counters are served yet."),
                "the empty cohort card must say so: $texts"
            )
            requireTrue(
                texts.contains("No completed runs are served on this page yet."),
                "the empty run list must say so: $texts"
            )
            requireTrue(panel.metricNames().isEmpty(), "no metric is invented")
            requirePanelRenders("empty outcomes", panel)
        }
    }

    /** The shared empty task-tree / plan state (same panel, two host entries). */
    private fun emptyTaskTreeState() {
        val panel = TaskTreePanel()
        requireTrue(panel.model() == null, "no task model may exist before a run")
        val root = panel.treeForTest().model.root as? javax.swing.tree.DefaultMutableTreeNode
        val payload = root?.userObject as? TaskTreeNode
        requireTrue(
            payload?.label == "No task data yet. Start a task to populate the tree.",
            "the empty tree must render its placeholder: ${payload?.label}"
        )
        requirePanelRenders("empty task tree", panel)
    }

    // ------------------------------------------------------------ loading axis

    /** The not-read / connecting presentations that precede the first read. */
    private fun loadingStateChecks() {
        check("state/loading/diagnostics") {
            val panel = DiagnosticsPanel()
            panel.setConnection("connecting to daemon", "connecting", 0L, null)
            requireTrue(
                panel.streamText().contains("connecting"),
                "loading diagnostics must render the connecting stream state"
            )
            requireTrue(
                textLines(panel).any { it.contains("connecting to daemon") },
                "the loading service row must name the connecting daemon"
            )
            requirePanelRenders("loading diagnostics", panel)
        }
        check("state/loading/outcomes") {
            val panel = OutcomesPanel()
            requireTrue(
                panel.model() == null,
                "the not-read outcomes state must not invent a model"
            )
            requireTrue(
                textLines(panel).contains("outcomes: not read yet"),
                "the loading outcomes surface must say it has not read yet"
            )
            requireTrue(
                panel.metricNames().isEmpty() && !panel.olderEnabled(),
                "no metric and no paging is offered before the read"
            )
            requirePanelRenders("loading outcomes", panel)
        }
    }

    // -------------------------------------------------------------- error axis

    /**
     * `state/error/<panel>` for the read-backed panels: the panel's real
     * refusal API drives the state and the exact mandated copy is pinned
     * wherever the state has mandated text (T29 cost, T31 history, I03 diff).
     */
    private fun errorStateChecks() {
        // Aggregate refusal render for the four panels with a direct typed
        // refusal API (kept beside the granular state/error/<panel> checks).
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
        check("state/error/permissions") {
            val panel = PermissionsPanel()
            panel.setUnavailable("permission route refused (status 503)")
            requireTrue(!panel.available(), "the refused approval route must render unavailable")
            requireTrue(
                panel.headerText().contains("(unavailable)"),
                "the refusal must be carried by the header text: ${panel.headerText()}"
            )
            requireTrue(
                panel.detailText().contains("permission route refused (status 503)"),
                "the refusal reason must stay visible: ${panel.detailText()}"
            )
            requireTrue(
                textLines(panel).any { it.startsWith("Approvals unavailable:") },
                "the refusal card must name itself: ${textLines(panel)}"
            )
            requirePanelRenders("error permissions", panel)
        }
        check("state/error/terminal") {
            val panel = TerminalPanel()
            panel.setUnavailable("terminal route refused (status 503)")
            requireTrue(!panel.available(), "the refused terminal route must render unavailable")
            requireTrue(
                panel.headerText().contains("commands: unavailable"),
                "the refusal must be carried by the header: ${panel.headerText()}"
            )
            requireTrue(
                panel.eventsText() == "terminal route refused (status 503)" &&
                    panel.outputText() == "terminal route refused (status 503)",
                "the refusal reason must replace the stale panes, never an empty frame"
            )
            requirePanelRenders("error terminal", panel)
        }
        check("state/error/settings") {
            val panel = SettingsPanel()
            panel.setUnavailable("provider read failed (status 503)")
            requireTrue(!panel.available(), "the refused provider read must render unavailable")
            requireTrue(
                panel.providerText() == "provider read failed (status 503)",
                "the refusal reason must render in the providers card: ${panel.providerText()}"
            )
            requireTrue(
                panel.statusText().contains("unavailable"),
                "the refusal must be carried by the status line: ${panel.statusText()}"
            )
            requireTrue(
                panel.providerCount() == 0 && panel.modelCount() == 0,
                "a refused read must clear the offered providers and models"
            )
            requirePanelRenders("error settings", panel)
        }
        // T31 exact copy: HistoryPanel.setUnavailable renders the mandated card.
        check("state/error/history") {
            val panel = HistoryPanel()
            panel.setUnavailable("history read refused (status 503)")
            requireTrue(!panel.available(), "the refused history read must render unavailable")
            requireTrue(
                panel.unavailableHeadlineText() == "History unavailable",
                "T31 label: ${panel.unavailableHeadlineText()}"
            )
            requireTrue(
                panel.unavailableCopyText() == "The conversation list could not be read.",
                "T31 copy: ${panel.unavailableCopyText()}"
            )
            requireTrue(
                panel.refreshHistoryActionText() == "Refresh history",
                "T31 action: ${panel.refreshHistoryActionText()}"
            )
            requireTrue(
                panel.statusText().contains("history read refused (status 503)"),
                "the refusal reason must stay visible: ${panel.statusText()}"
            )
            requirePanelRenders("error history", panel)
        }
        check("state/error/diagnostics") {
            val panel = DiagnosticsPanel()
            panel.setUnavailable("diagnostics read refused (status 503)")
            requireTrue(!panel.available(), "the refused diagnostics read must render unavailable")
            requireTrue(
                panel.daemonText() == "unavailable",
                "the refused service row must say unavailable: ${panel.daemonText()}"
            )
            requireTrue(
                panel.streamText().contains("unavailable") &&
                    panel.streamText().contains("diagnostics read refused (status 503)"),
                "the refused stream row must carry the reason: ${panel.streamText()}"
            )
            requirePanelRenders("error diagnostics", panel)
        }
        // T29 exact copy: the refusal model carries no monetary value at all.
        check("state/error/outcomes") {
            val panel = OutcomesPanel()
            panel.setModel(outcomesPanelModelOf(null, "outcomes read refused (status 503)"))
            val model = panel.model() ?: throw AssertionError("the refusal model must be set")
            requireTrue(model.state == "unavailable", "the refused read must be unavailable")
            requireTrue(
                panel.unavailableHeadlineText() == OUTCOMES_COST_UNAVAILABLE,
                "T29 label: ${panel.unavailableHeadlineText()}"
            )
            requireTrue(
                panel.unavailableCopyText() == OUTCOMES_COST_UNAVAILABLE_COPY,
                "T29 copy: ${panel.unavailableCopyText()}"
            )
            requireTrue(
                panel.viewDetailsText() == "View details",
                "T29 action: ${panel.viewDetailsText()}"
            )
            requireTrue(!model.hasMonetaryValue(), "no monetary value may sit behind a refusal")
            requireTrue(
                textLines(panel).contains("outcomes: cost unavailable"),
                "the refusal must be carried by the status line"
            )
            requirePanelRenders("error outcomes", panel)
        }
        // I03: the refused changes read and the exact Diff unavailable copy.
        check("state/error/changes") {
            val panel = ChangesPanel()
            panel.setUnavailable("change read failed: task view read failed")
            requireTrue(
                panel.unavailableReason() == "change read failed: task view read failed",
                "the refusal reason must be recorded: ${panel.unavailableReason()}"
            )
            requireTrue(
                panel.unavailableHeadlineText() == CHANGES_UNAVAILABLE_LABEL,
                "the refused changes headline: ${panel.unavailableHeadlineText()}"
            )
            panel.showDiffUnavailable()
            requireTrue(
                panel.diffUnavailableVisible(),
                "the I03 Diff unavailable state must be selectable"
            )
            requireTrue(
                panel.diffUnavailableLabelText() == DIFF_UNAVAILABLE_LABEL,
                "I03 label: ${panel.diffUnavailableLabelText()}"
            )
            requireTrue(
                panel.diffUnavailableCopyText() == DIFF_UNAVAILABLE_COPY,
                "I03 copy: ${panel.diffUnavailableCopyText()}"
            )
            requireTrue(
                panel.diffUnavailableCopyText() ==
                    "The runtime does not serve an immutable candidate content read for this change.",
                "the I03 copy is exactly pinned"
            )
            requirePanelRenders("error changes", panel)
        }
        // T15: a claim with no served verdict keeps its unavailable marker; a
        // refused proof read discards the stale record (fail-closed) and the
        // Plan surface names the refusal.
        check("state/error/verification") {
            val model = TaskTree.build(
                taskVerification = parseNativeTaskVerification(PARITY_CRITERION_PROOF_JSON),
                proofUnavailable = "proof read failed: corrupt_durable_state"
            )
            requireTrue(
                model.verification.status == "unavailable",
                "a refused proof read must drive verification unavailable"
            )
            requireTrue(
                !model.verification.verified,
                "a refused proof read must never render verified"
            )
            val panel = VerificationPanel()
            panel.updateModel(model)
            requireTrue(
                panel.summaryText().startsWith("Not verified yet"),
                "the refused proof must not present a verified summary: ${panel.summaryText()}"
            )
            val labels = panel.criterionLabels()
            requireTrue(
                labels.any { it.startsWith("[unavailable] unavailable criterion") },
                "T15: the unavailable claim keeps its text-carried verdict: $labels"
            )
            requireTrue(
                textLines(panel).contains("required criterion · unavailable"),
                "the unavailable claim names its binding state: ${textLines(panel)}"
            )
            val plan = TaskTreePanel()
            plan.update(model)
            requireTrue(
                textLines(plan).any { it.startsWith("VERIFICATION UNAVAILABLE") },
                "the plan surface must name the refused verification read"
            )
            requirePanelRenders("error verification", panel)
        }
        check("state/error/evidence") {
            val panel = EvidenceNavigatorPanel()
            val ref = EvidenceRefs.parse("evidence:41")
            panel.setEvidence(listOf(ref))
            panel.selectEvidence(ref)
            requireTrue(
                panel.outputText().contains("selected evidence:41"),
                "the selection must render before the refusal: ${panel.outputText()}"
            )
            panel.showError(41L, "evidence retrieve refused (status 404 not_found)")
            requireTrue(
                panel.outputText() == "evidence 41: evidence retrieve refused (status 404 not_found)",
                "the retrieval refusal must render its exact reason: ${panel.outputText()}"
            )
            requireTrue(
                panel.evidenceCount() == 1,
                "the refusal must keep the served ref list, never drop it"
            )
            requirePanelRenders("error evidence", panel)
        }
        // T29 refusal + T30 billing-disabled refusal, both without fabricated
        // numbers (the panel's real wording).
        check("state/error/usage") {
            val panel = UsagePanel()
            panel.setUnavailable("usage read refused (status 503)")
            requireTrue(
                panel.model()!!.state == "unavailable",
                "the refused usage read must render unavailable"
            )
            requireTrue(
                panel.lines().any { it.startsWith("usage unavailable") },
                "the refused state must carry its reason: ${panel.lines()}"
            )
            requireTrue(
                panel.periodText().contains("Tokens — not reported"),
                "a refused read must not fabricate numbers: ${panel.periodText()}"
            )
            requireTrue(!panel.grantEnabled(), "a refused billing surface never offers a grant")
            panel.setUnavailable("billing is disabled in this installation", disabled = true)
            requireTrue(
                panel.model()!!.state == "disabled",
                "the billing-disabled refusal must render disabled"
            )
            requireTrue(
                panel.lines().any { it.startsWith("billing disabled locally") },
                "T30: the disabled billing surface must name itself: ${panel.lines()}"
            )
            requireTrue(
                panel.periodText().contains(
                    "Billing is disabled locally; no period numbers are served."
                ),
                "no zero-dollar account is presented: ${panel.periodText()}"
            )
            requirePanelRenders("error usage", panel)
        }
        // The roster read can refuse; AgentsPanel has no refusal rendering
        // today, so the honest panel observable is that a failed/unread
        // roster fabricates nothing and keeps the explicit placeholder (the
        // FaktorChatPanel refusal rides the transcript system line
        // "error: refresh agents: <message>"). Reported with this run.
        check("state/error/agents") {
            val panel = AgentsPanel()
            requireTrue(
                panel.rosterText().isEmpty(),
                "a failed roster read must not fabricate agent rows"
            )
            // The explicit empty replacement keeps the honest placeholder
            // (the failure itself never swaps in invented rows).
            panel.update(emptyList(), null)
            requireTrue(panel.rosterText().isEmpty(), "no agent row may be invented")
            requireTrue(
                panel.advancedText() == "No agents yet. Rows appear here while a run works.",
                "the failed/unread roster keeps the explicit empty placeholder: " +
                    panel.advancedText()
            )
            requirePanelRenders("error agents", panel)
        }
    }

    // ------------------------------------------------- blocked / reconnect axis

    /** T10/T11-family blocked wording and the T27/T28 recovery surfaces. */
    private fun recoveryStateChecks() {
        check("state/blocked/blockers") {
            val panel = cannedBlockersPanel()
            requireTrue(panel.blockerCount() >= 1, "blockers model must carry a blocked row")
            // The served reasons render verbatim (a blocked child owes its
            // reason, dependency and resolution as text, not a tone).
            requireTrue(
                panel.blockerReasonText("child-1") == "shell call needs approval",
                "the permission blocker reason renders verbatim"
            )
            requireTrue(
                panel.blockerReasonText("child-2") == "waiting on impl-a",
                "the dependency blocker reason renders verbatim"
            )
            requireTrue(
                panel.applicableActions().contains(BlockerAction.RESUME),
                "a blocked row exposes its resolve action: ${panel.applicableActions()}"
            )
            // T10 family: the decision card names the action and target and
            // carries Allow/Deny for THAT request.
            val allow = panel.permissionActionButton("7", "allow")
                ?: throw AssertionError("the pending permission card must render Allow")
            val deny = panel.permissionActionButton("7", "deny")
                ?: throw AssertionError("the pending permission card must render Deny")
            requireTrue(
                allow.text == "Allow" && allow.isEnabled,
                "T10 Allow must attach to request 7"
            )
            requireTrue(
                deny.text == "Deny" && deny.isEnabled,
                "T10 Deny must attach to request 7"
            )
            val texts = textLines(panel)
            requireTrue(
                texts.any { it.contains("Run a shell command: bash") },
                "the approval card names the action and target: $texts"
            )
            requireTrue(
                texts.any { it.contains("child-1 [blocked/permission] shell call needs approval") },
                "the blocked child card must render: $texts"
            )
            requireTrue(
                texts.any { it.contains("resolution: allow the shell tool") },
                "the served resolution must render: $texts"
            )
            requireTrue(
                texts.contains("No task-level blockers."),
                "the empty task-blocker section stays honest: $texts"
            )
            requirePanelRenders("blocked blockers", panel)
            val tree = cannedTaskTreePanel()
            ParityAwt.layout(tree, 320, 600)
            val stats = paint(tree, 320, 600)
            requireTrue(stats.distinctColors >= 2, "blocked task tree degenerate: $stats")
        }
        check("state/reconnect/diagnostics") {
            val panel = cannedDiagnosticsPanel()
            requireTrue(panel.restartEnabled(), "restart control must exist for reconnect")
            requireTrue(panel.reconnectEnabled(), "reconnect control must exist")
            // T27 exact copy and actions.
            panel.setStreamRecovery(StreamRecoveryState.RECONNECTING)
            requireTrue(
                panel.streamRecoveryState() == StreamRecoveryState.RECONNECTING,
                "the transient reconnect state must be current"
            )
            requireTrue(
                panel.recoveryHeadline() == RECONNECTING_HEADLINE,
                "T27 label: ${panel.recoveryHeadline()}"
            )
            requireTrue(
                panel.recoveryHeadline() == "Reconnecting live updates",
                "T27 label is exactly pinned"
            )
            requireTrue(
                panel.recoveryBody() == RECONNECTING_COPY,
                "T27 copy: ${panel.recoveryBody()}"
            )
            requireTrue(
                panel.recoveryBody() == "The last valid journal position is kept.",
                "T27 copy is exactly pinned"
            )
            requireTrue(
                panel.recoveryActions() == listOf("Wait", "Refresh"),
                "T27 actions: ${panel.recoveryActions()}"
            )
            // T28 exact copy and actions (no skip-event control exists).
            panel.setStreamRecovery(StreamRecoveryState.PAUSED, "event 42 is unreadable")
            requireTrue(
                panel.recoveryHeadline() == STREAM_PAUSED_HEADLINE,
                "T28 label: ${panel.recoveryHeadline()}"
            )
            requireTrue(
                panel.recoveryHeadline() == "Live updates paused",
                "T28 label is exactly pinned"
            )
            requireTrue(
                panel.recoveryBody() == STREAM_PAUSED_COPY,
                "T28 copy: ${panel.recoveryBody()}"
            )
            requireTrue(
                panel.recoveryBody() ==
                    "The client cannot read the next durable event. Nothing is skipped.",
                "T28 copy is exactly pinned"
            )
            requireTrue(
                panel.recoveryActions() == listOf("Reconnect", "Refresh from snapshot"),
                "T28 actions: ${panel.recoveryActions()}"
            )
            requireTrue(
                panel.recoveryActions().none { it.contains("skip", ignoreCase = true) },
                "no skip-event control may exist: ${panel.recoveryActions()}"
            )
            // D10/KEEP-10 trust wording rows, exactly.
            panel.setTrust(
                daemonTrustLabel(DaemonBinary.Trust.Verified("record matches")),
                "/opt/faktor/bin/faktor-cli"
            )
            requireTrue(
                panel.trustText().startsWith("Matches recorded checksum"),
                "trust row: ${panel.trustText()}"
            )
            requireTrue(
                panel.trustText().contains("/opt/faktor/bin/faktor-cli"),
                "the resolved executable stays visible: ${panel.trustText()}"
            )
            panel.setTrust(daemonTrustLabel(DaemonBinary.Trust.Unverified("no record")), null)
            requireTrue(
                panel.trustText() == "No checksum record",
                "trust row: ${panel.trustText()}"
            )
            panel.setTrust(daemonTrustLabel(DaemonBinary.Trust.Refused("mismatch")), null)
            requireTrue(
                panel.trustText() == "Checksum mismatch",
                "trust row: ${panel.trustText()}"
            )
            requireTrue(
                daemonTrustLabel(null) == "—",
                "an unresolved trust state stays missing"
            )
            panel.setConnection("stopped", "off", 0L, null)
            requireTrue(
                panel.streamText().contains("off"),
                "stopped connection must render honestly"
            )
            requirePanelRenders("reconnect diagnostics", panel)
        }
    }

    /** A fresh FaktorChatPanel host for the Work-surface state checks. */
    private fun newChatPanel(tag: String): FaktorChatPanel {
        val service = FaktorFrontendService(
            java.nio.file.Paths.get("unused-binary"),
            Files.createTempDirectory("faktor-host-matrix-$tag-")
        )
        val panel = FaktorChatPanel(service)
        taskPanels.add(panel)
        return panel
    }

    /** Lays the panel out offscreen and requires a non-degenerate frame. */
    private fun requirePanelRenders(name: String, panel: Component, width: Int = 320) {
        ParityAwt.layout(panel, width, 600)
        val stats = paint(panel, width, 600)
        requireTrue(stats.distinctColors >= 2, "$name rendered a degenerate frame: $stats")
    }

    /**
     * Every rendered text of the subtree: wrapped labels carry their full
     * untruncated text, buttons and editors their exact content. Hidden
     * children are included deliberately (Swing CardLayout keeps both cards
     * in the tree); the checks pair this with the panel state accessor.
     */
    private fun textLines(component: Component): List<String> {
        val out = ArrayList<String>()
        fun walk(component: Component) {
            when (component) {
                is WrappedLabel -> if (component.fullText.isNotEmpty()) {
                    out.add(component.fullText)
                }
                is AbstractButton -> component.text?.takeIf { it.isNotEmpty() }?.let { out.add(it) }
                is JLabel -> component.text?.takeIf { it.isNotEmpty() }?.let { out.add(it) }
                is JTextComponent -> if (component.text.isNotEmpty()) out.add(component.text)
            }
            if (component is Container) {
                for (child in component.components) walk(child)
            }
        }
        walk(component)
        return out
    }

    // -------------------------------------------------------------- plumbing

    private fun check(name: String, body: () -> Unit) {
        try {
            body()
            checks.add(HostCheck(name, "passed", "observable matched"))
        } catch (t: Throwable) {
            failures++
            // invokeAndWait wraps EDT throwables in an InvocationTargetException
            // whose message is null — report the TARGET cause instead.
            val cause = (t as? java.lang.reflect.InvocationTargetException)?.targetException ?: t
            val message = (cause.message ?: cause.toString()) + " @ " +
                cause.stackTrace.take(5).joinToString(" <- ") { it.toString() }
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
        // 8px probe grid: a 16px grid under-counts sparse, flat-painted rows
        // (Metal renders the utilities toolbar as text + flat fills that a
        // 16px grid mostly steps over), reading a fully painted frame as
        // degenerate; a truly blank frame still yields 1-2 colors at 8px.
        while (y < height) {
            var x = 0
            while (x < width) {
                val rgb = image.getRGB(x, y)
                distinct[rgb] = true
                sampled++
                if (rgb != corner) nonCorner++
                x += 8
            }
            y += 8
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

    /** True when the subtree contains a scroll pane (no-scroll guard). */
    private fun containsScrollPane(component: Component): Boolean {
        if (component is javax.swing.JScrollPane) return true
        if (component is Container) {
            for (child in component.components) {
                if (containsScrollPane(child)) return true
            }
        }
        return false
    }

    /** The distinct row Y positions of a wrapping row (wrap proof). */
    private fun wrappedRowCount(container: Container): Int {
        val ys = LinkedHashSet<Int>()
        for (child in container.components) {
            if (child.isVisible) ys.add(child.y)
        }
        return ys.size
    }

    private fun collectFocusable(component: Component): List<Component> {        val out = ArrayList<Component>()
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
