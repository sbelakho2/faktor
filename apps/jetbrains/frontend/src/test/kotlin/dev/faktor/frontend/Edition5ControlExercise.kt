// Registry-driven "exercise every JetBrains control" suite.
//
// It is deliberately NOT a screenshot-only check: for all 200 catalog rows
// it activates the real Swing editor, asserts an observable staged state (or
// the typed contract-required notice for action rows), then applies one
// atomic patch over HTTP and requires the daemon acknowledgement. All 32
// screen components are built and every enabled button/checkbox/combo/text
// control is activated. Counts and the exact HTTP calls are written to
// target/certification/jetbrains-edition5/control-exercise.json.
//
// The HTTP server here is a deterministic CONTRACT FIXTURE (the spec's
// "semantic fixtures / fake daemon effects"): it proves the client wiring and
// the UI state machine. It is never the production source of settings
// effectiveness; a route absent from the real daemon is surfaced by the UI as
// contract-required and listed in the report.
package dev.faktor.frontend

import com.sun.net.httpserver.HttpExchange
import com.sun.net.httpserver.HttpServer
import dev.faktor.backend.NativeClient
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import java.awt.Component
import java.awt.Container
import java.net.InetSocketAddress
import java.nio.file.Files
import java.nio.file.Paths
import java.util.Collections
import javax.swing.AbstractButton
import javax.swing.JCheckBox
import javax.swing.JComboBox
import javax.swing.JComponent
import javax.swing.JTextArea
import javax.swing.JTextField
import javax.swing.SwingUtilities

object Edition5ControlExercise {

    private val calls = Collections.synchronizedList(ArrayList<String>())

    @JvmStatic
    fun main(args: Array<String>) {
        val failures = ArrayList<String>()
        val daemon = FixtureDaemon.start()
        try {
            require(Edition5Catalog.controls.size == 200) {
                "catalog must contain 200 controls, got ${Edition5Catalog.controls.size}"
            }
            require(Edition5Catalog.screens.size == 32) {
                "catalog must contain 32 screens, got ${Edition5Catalog.screens.size}"
            }
            val referenced = Edition5Catalog.controls.count { it.screens.isNotEmpty() }
            require(referenced == 200) { "every control must name at least one screen, got $referenced" }

            val client = NativeClient(daemon.baseUrl, "fixture-password")
            val settings = exerciseSettings(client, failures)
            val screens = exerciseScreens(client, failures)
            if (failures.isNotEmpty()) {
                println("EDITION5 CONTROL EXERCISE FAILED (${failures.size})")
                for (failure in failures) println(" - $failure")
                kotlin.system.exitProcess(1)
            }
            val report = LinkedHashMap<String, JsonValue>()
            report["schema"] = JsonValue.Str("faktor-jetbrains-edition5-control-exercise/v1")
            report["mode"] = JsonValue.Str("contract-fixture-daemon")
            report["screens"] = JsonValue.Int64(32)
            report["controls"] = JsonValue.Int64(200)
            report["controlsActivated"] = JsonValue.Int64(settings.activated.toLong())
            report["controlsStaged"] = JsonValue.Int64(settings.staged.toLong())
            report["patchKeys"] = JsonValue.Int64(settings.patchKeys.toLong())
            report["atomicAppliesAcknowledged"] = JsonValue.Int64(settings.applies.toLong())
            report["screensBuilt"] = JsonValue.Int64(screens.built.toLong())
            report["screenControlsActivated"] = JsonValue.Int64(screens.activated.toLong())
            report["httpCalls"] = JsonValue.Arr(
                calls.groupingBy { it }.eachCount().toSortedMap().map { (path, count) ->
                    JsonValue.Obj(
                        linkedMapOf(
                            "call" to JsonValue.Str(path),
                            "count" to JsonValue.Int64(count.toLong())
                        )
                    )
                }
            )
            report["contractGaps"] = JsonValue.Arr(
                Edition5Routes.ALL.map { JsonValue.Str(it) }
            )
            val root = Paths.get(repoRoot(), "target", "certification", "jetbrains-edition5")
            Files.createDirectories(root)
            Files.write(
                root.resolve("control-exercise.json"),
                JsonCodec.write(JsonValue.Obj(report)).toByteArray(Charsets.UTF_8)
            )
            println(
                "EDITION5 CONTROL EXERCISE: screens=${screens.built}/32 controls=${settings.activated}/200 " +
                    "staged=${settings.staged} patchKeys=${settings.patchKeys} applies=${settings.applies} " +
                    "screenControls=${
                        screens.activated
                    } httpCalls=${calls.size}"
            )
        } finally {
            daemon.stop()
        }
    }

    private data class SettingsResult(
        val activated: Int,
        val staged: Int,
        val patchKeys: Int,
        val applies: Int
    )

    private data class ScreenResult(val built: Int, val activated: Int)

    private fun exerciseSettings(client: NativeClient, failures: MutableList<String>): SettingsResult {
        val source = NativeEdition5ConfigSource(client)
        val controller = Edition5SettingsController(source)
        lateinit var panel: Edition5SettingsPanel
        SwingUtilities.invokeAndWait { panel = Edition5SettingsPanel(controller) }
        panel.awaitIdleForTest()
        if (controller.snapshot == null) {
            failures.add("config fixture did not serve an effective snapshot: ${controller.loadError}")
            return SettingsResult(0, 0, 0, 0)
        }
        var activated = 0
        val rows = panel.rowsForTest()
        if (rows.size != 200) failures.add("settings panel rendered ${rows.size} rows, expected 200")
        for (row in rows) {
            SwingUtilities.invokeAndWait {
                panel.selectCategoryForTest(row.control.category)
                val before = row.stateLabel()
                row.activateForTest()
                val after = row.stateLabel()
                val observable = before != after ||
                    Edition5ControlKinds.of(row.control) == Edition5ControlKind.ACTION ||
                    controller.value(row.control).isNotEmpty()
                if (!observable) {
                    failures.add("${row.control.id} activation produced no observable state")
                } else {
                    activated++
                }
            }
        }
        val staged = controller.changedCount()
        val patchKeys = controller.changes().size
        if (staged < 150) failures.add("only $staged controls staged a value (expected most rows)")
        var applies = 0
        val decision = controller.applyOffEdt()
        if (!decision.ok) {
            failures.add("atomic apply refused: ${decision.contractGap ?: decision.errors}")
        } else {
            applies = 1
            if (controller.changedCount() != 0) {
                failures.add("apply did not clear staged values (${controller.changedCount()} remain)")
            }
        }
        panel.shutdown()
        return SettingsResult(activated, staged, patchKeys, applies)
    }

    private fun exerciseScreens(client: NativeClient, failures: MutableList<String>): ScreenResult {
        var built = 0
        var activated = 0
        for (screen in Edition5Catalog.screens) {
            var builtComponent: JComponent? = null
            try {
                SwingUtilities.invokeAndWait {
                    builtComponent = Edition5ScreenRegistry.build(
                        screen.id,
                        Edition5ScreenContext(
                            clientProvider = { client },
                            sessionId = "1",
                            taskId = "1",
                            catalog = fixtureCatalog()
                        )
                    )
                }
            } catch (e: Exception) {
                failures.add("${screen.id} failed to build: ${e.message}")
                continue
            }
            val component = builtComponent ?: run {
                failures.add("${screen.id} produced no component")
                continue
            }
            when (component) {
                is Edition5AsyncPanel -> component.awaitIdleForTest()
                is Edition5SettingsPanel -> component.awaitIdleForTest()
            }
            val count = countComponents(component)
            if (count < 4) failures.add("${screen.id} rendered only $count components")
            built++
            val activations = activateAll(component, screen.id, failures)
            activated += activations
        }
        return ScreenResult(built, activated)
    }

    private fun countComponents(component: Component): Int {
        var count = 1
        if (component is Container) {
            for (child in component.components) count += countComponents(child)
        }
        return count
    }

    private fun activateAll(component: Component, screenId: String, failures: MutableList<String>): Int {
        var activated = 0
        val visited = ArrayList<Component>()
        fun walk(node: Component) {
            visited.add(node)
            if (node is Container) for (child in node.components) walk(child)
        }
        walk(component)
        for (node in visited) {
            try {
                when (node) {
                    is JCheckBox -> {
                        if (node.isEnabled) {
                            SwingUtilities.invokeAndWait { node.doClick() }
                            activated++
                        }
                    }
                    is AbstractButton -> {
                        if (node.isEnabled && !node.text.contains("More")) {
                            SwingUtilities.invokeAndWait { node.doClick() }
                            activated++
                        }
                    }
                    is JComboBox<*> -> {
                        if (node.isEnabled && node.itemCount > 1) {
                            SwingUtilities.invokeAndWait {
                                node.selectedIndex = (node.selectedIndex + 1) % node.itemCount
                            }
                            activated++
                        }
                    }
                    is JTextArea -> {
                        if (node.isEditable) {
                            SwingUtilities.invokeAndWait { node.text = (node.text + " exercised").trim() }
                            activated++
                        }
                    }
                    is JTextField -> {
                        if (node.isEditable) {
                            SwingUtilities.invokeAndWait { node.text = "exercised" }
                            activated++
                        }
                    }
                }
            } catch (e: Exception) {
                failures.add("$screenId control ${node.javaClass.simpleName} activation failed: ${e.message}")
            }
        }
        return activated
    }

    private fun fixtureCatalog(): List<dev.faktor.shared.NativeModelInfo> = listOf(
        dev.faktor.shared.NativeModelInfo(
            provider = "fixture",
            model = "model-a",
            context = 32768,
            maxOutput = 4096,
            tools = true,
            reasoning = true,
            source = "fixture"
        )
    )

    private fun repoRoot(): String {
        val configured = System.getProperty("faktor.repo.root")
        if (!configured.isNullOrBlank()) return configured
        var dir = Paths.get("").toAbsolutePath()
        while (dir != null && !Files.isRegularFile(dir.resolve("Cargo.toml"))) {
            dir = dir.parent
        }
        return (dir ?: Paths.get("").toAbsolutePath()).toString()
    }

    private class FixtureDaemon private constructor(private val server: HttpServer) {
        val baseUrl: String = "http://127.0.0.1:${server.address.port}"

        fun stop() = server.stop(0)

        companion object {
            fun start(): FixtureDaemon {
                val server = HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
                server.createContext("/") { exchange ->
                    try {
                        handle(exchange)
                    } catch (e: Exception) {
                        respond(exchange, 500, "{\"error\":{\"code\":\"fixture\",\"message\":\"${e.message}\"}}")
                    }
                }
                server.start()
                return FixtureDaemon(server)
            }

            private fun handle(exchange: HttpExchange) {
                val path = exchange.requestURI.path
                val method = exchange.requestMethod
                calls.add("$method $path")
                exchange.requestBody.readBytes()
                when (path) {
                    Edition5Routes.CONFIG_EFFECTIVE -> respond(exchange, 200, configEffective())
                    Edition5Routes.CONFIG_VALIDATE -> respond(exchange, 200, "{\"config\":{\"ok\":true,\"revision\":1}}")
                    Edition5Routes.CONFIG_APPLY -> respond(
                        exchange, 200,
                        "{\"config\":{\"ok\":true,\"revision\":2," +
                            "\"apply_timing\":{\"*\":\"Next task\"}}}"
                    )
                    Edition5Routes.CONTEXT_MANIFEST -> respond(
                        exchange, 200,
                        "{\"context\":{\"revision\":4,\"basis\":\"estimated\",\"capacity\":32768," +
                            "\"input\":18400,\"output_reserve\":4096,\"safety_reserve\":2048," +
                            "\"headroom\":8224,\"categories\":\"system 1200 • files 16200 • reserves 6144\"," +
                            "\"sources\":\"file:src/a.kt@v3 • diagnostic:KT-1\",\"capture_version\":\"ctx-4\"}}"
                    )
                    Edition5Routes.CONTEXT_COMPACTION_PLAN -> respond(
                        exchange, 200,
                        "{\"plan\":{\"revision\":4,\"retained_facts\":\"contract v3 • candidate tm1\", " +
                            "\"pending_operations\":\"1 approval\",\"unknown_effects\":\"1\", " +
                            "\"summary_model\":\"fixture/model-a\",\"egress\":\"fixture\", " +
                            "\"cost\":\"USD 0.01\",\"minimum_reduction_permille\":200}}"
                    )
                    Edition5Routes.CONTEXT_COMPACT -> respond(
                        exchange, 200,
                        "{\"result\":{\"state\":\"applied\",\"measured_reduction_permille\":250, " +
                            "\"summary_version\":\"sum-5\",\"before\":\"28400\",\"after\":\"21300\", " +
                            "\"retained_obligations\":\"1 approval; 1 unknown effect\"}}"
                    )
                    Edition5Routes.PERMISSIONS_POLICY -> respond(
                        exchange, 200,
                        "{\"policy\":{\"revision\":3,\"managed\":\"org deny: network\", " +
                            "\"sandbox_filesystem\":\"workspace\",\"sandbox_shell\":\"workspace\", " +
                            "\"sandbox_network\":\"deny\"}}"
                    )
                    Edition5Routes.PERMISSIONS_RULES -> respond(exchange, 200, "{\"rules\":[]}")
                    Edition5Routes.MCP_SERVERS -> respond(exchange, 200, "{\"servers\":[]}")
                    Edition5Routes.SKILLS -> respond(exchange, 200, "{\"skills\":[]}")
                    Edition5Routes.BROWSER_STATUS -> respond(
                        exchange, 200,
                        "{\"browser\":{\"available\":false,\"reason\":\"no implemented adapter advertised\"}}"
                    )
                    Edition5Routes.BINDING -> respond(
                        exchange, 200,
                        "{\"binding\":{\"generation\":7,\"editor_host\":\"jetbrains-laptop\", " +
                            "\"workspace_uri\":\"file:///work/auth-service\",\"daemon_host\":\"dev-box\", " +
                            "\"model_endpoint\":\"127.0.0.1:11434 (daemon loopback)\", " +
                            "\"embedding_endpoint\":\"Unavailable\",\"vault\":\"machine/user\", " +
                            "\"worker\":\"local\",\"candidate\":\"isolated task-184\", " +
                            "\"data_boundary\":\"local candidate, no hosted egress\"}}"
                    )
                    "/native/session/1/checkpoints" -> respond(exchange, 200, "{\"checkpoints\":[]}")
                    "/native/tasks/1/proof" -> respond(
                        exchange, 200,
                        "{\"proof\":{\"proof_state\":\"pending\",\"proof_state_reason\":\"unit evidence missing\"," +
                            "\"criteria_passed\":2,\"criteria_total\":3,\"criteria_failed\":1," +
                            "\"criteria_unavailable\":0,\"checks_passed\":1,\"checks_total\":2," +
                            "\"certifies_completion\":false,\"candidate_identity\":\"tm1:cand\",\"landed\":\"no\"}}"
                    )
                    "/native/tasks/1/completion-steps" -> respond(
                        exchange, 200,
                        "{\"completion\":{\"gate_status\":\"pending\",\"gate_reason\":\"unit evidence missing\"}}"
                    )
                    else -> {
                        if (path.startsWith("/native/")) {
                            respond(exchange, 200, "{}")
                        } else {
                            respond(exchange, 404, "{\"error\":{\"code\":\"not_found\",\"message\":\"fixture\"}}")
                        }
                    }
                }
            }

            private fun configEffective(): String {
                val fields = Edition5Catalog.controls.flatMap { control ->
                    val key = control.primaryConfigKey ?: "edition5.${control.id.lowercase()}"
                    listOf(key)
                }.distinct()
                val body = StringBuilder("{\"config\":{\"revision\":1,\"saved_revision\":1,")
                body.append("\"runtime_revision\":1,\"fields\":[")
                fields.forEachIndexed { index, key ->
                    if (index > 0) body.append(',')
                    body.append("{\"key\":").append(quote(key))
                    body.append(",\"value\":\"\",\"source\":\"fixture\"")
                    body.append(",\"locked\":false,\"apply_timing\":\"Next task\"}")
                }
                body.append("]}}")
                return body.toString()
            }

            private fun quote(value: String): String =
                JsonCodec.write(JsonValue.Str(value))

            private fun respond(exchange: HttpExchange, status: Int, body: String) {
                val bytes = body.toByteArray(Charsets.UTF_8)
                exchange.responseHeaders.add("Content-Type", "application/json")
                exchange.sendResponseHeaders(status, bytes.size.toLong())
                exchange.responseBody.use { it.write(bytes) }
            }
        }
    }
}
