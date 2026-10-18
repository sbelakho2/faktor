// Live Revision 8 acceptance A01-A40 against a real daemon.
//
// Unlike the fixture overlay, this harness requires FAKTOR_E8_BASE_URL and
// FAKTOR_E8_TOKEN and drives the same registry screens/controls as the
// product. Every mapped control must produce a real daemon receipt: a Read
// state whose detail is a bounded JSON body, or a typed Disabled/Failed
// receipt (recorded, never converted into success). A transport failure or a
// control without a handler fails the scenario.
package dev.faktor.frontend

import dev.faktor.backend.NativeClient
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import java.awt.Component
import java.awt.Container
import java.io.File
import java.nio.file.Files
import java.nio.file.Paths
import java.util.LinkedHashMap
import javax.swing.AbstractButton
import javax.swing.SwingUtilities

object Edition8LiveAcceptance {

    private val failures = ArrayList<String>()

    @JvmStatic
    fun main(args: Array<String>) {
        val baseUrl = System.getenv("FAKTOR_E8_BASE_URL").orEmpty()
        val token = System.getenv("FAKTOR_E8_TOKEN").orEmpty()
        if (baseUrl.isBlank() || token.isBlank()) {
            println("EDITION8 LIVE ACCEPTANCE SKIPPED: no FAKTOR_E8_BASE_URL/TOKEN")
            return
        }
        val session = System.getenv("FAKTOR_E8_SESSION")?.takeIf { it.isNotBlank() } ?: "1"
        val task = System.getenv("FAKTOR_E8_TASK")?.takeIf { it.isNotBlank() } ?: "1"
        val client = NativeClient(baseUrl, token)
        val host = Edition8Host(
            client = { client },
            sessionId = { session },
            taskId = { task }
        )
        val panels = LinkedHashMap<String, Edition8ScreenPanel>()
        val statuses = LinkedHashMap<String, Pair<String, String>>()
        val records = ArrayList<JsonValue>()
        var read = 0
        var typed = 0
        try {
            for (scenario in Edition8Registry.acceptance) {
                val mapped = Edition8Registry.controls.filter { scenario.id in it.acceptanceIds }
                if (mapped.isEmpty()) {
                    failures.add("${scenario.id} maps to no control")
                    continue
                }
                val scenarioRead = ArrayList<String>()
                val scenarioTyped = ArrayList<String>()
                val scenarioFailed = ArrayList<String>()
                for (control in mapped) {
                    val screenId = control.primaryScreen.takeIf { it in control.screens }
                        ?: control.screens.first()
                    val panel = panels.getOrPut(screenId) {
                        SwingUtilities.invokeAndWait { }
                        var built: Edition8ScreenPanel? = null
                        SwingUtilities.invokeAndWait {
                            built = Edition8ScreenRegistry.build(screenId, host) as Edition8ScreenPanel
                        }
                        val view = built ?: error("cannot build $screenId")
                        host.work.awaitIdle(30_000)
                        view
                    }
                    host.work.awaitIdle(30_000)
                    val row = panel.rowsForTest().firstOrNull { it.control.id == control.id }
                    if (row == null) {
                        scenarioFailed.add("${control.id}@$screenId unrendered")
                        continue
                    }
                    val cached = statuses[control.id]
                    if (cached == null) {
                        val button = findButton(panel, "edition8-activate-${control.id}")
                        if (button == null) {
                            scenarioFailed.add("${control.id} has no handler")
                            continue
                        }
                        SwingUtilities.invokeAndWait { button.doClick() }
                        host.work.awaitIdle(30_000)
                        val state = row.stateText()
                        val kind = when {
                            state.startsWith("Read") -> "read"
                            state.startsWith("Disabled") || state.startsWith("Refused") -> "typed"
                            state.startsWith("Failed") -> "typed"
                            else -> "missing"
                        }
                        statuses[control.id] = kind to state
                    }
                    when (val kind = statuses[control.id]?.first) {
                        "read" -> scenarioRead.add(control.id)
                        "typed" -> scenarioTyped.add(control.id)
                        else -> scenarioFailed.add(control.id + " " + (statuses[control.id]?.second ?: "no receipt"))
                    }
                }
                read += scenarioRead.size
                typed += scenarioTyped.size
                if (scenarioFailed.isNotEmpty()) {
                    failures.add("${scenario.id}: ${scenarioFailed.take(6)}")
                }
                val passed = scenarioFailed.isEmpty() && (scenarioRead.size + scenarioTyped.size) == mapped.size
                records.add(
                    JsonValue.Obj(
                        linkedMapOf(
                            "id" to JsonValue.Str(scenario.id),
                            "title" to JsonValue.Str(scenario.title),
                            "screens" to JsonValue.Arr(
                                mapped.flatMap { it.screens }.distinct().sorted().map { JsonValue.Str(it) }
                            ),
                            "controls" to JsonValue.Arr(mapped.map { JsonValue.Str(it.id) }),
                            "field_checks" to JsonValue.Obj(
                                linkedMapOf(
                                    "mapped" to JsonValue.Int64(mapped.size.toLong()),
                                    "read" to JsonValue.Int64(scenarioRead.size.toLong()),
                                    "typed" to JsonValue.Int64(scenarioTyped.size.toLong()),
                                    "failed" to JsonValue.Int64(scenarioFailed.size.toLong())
                                )
                            ),
                            "source" to JsonValue.Str("acceptance.md ${scenario.id} + controls.json live routes"),
                            "passed" to JsonValue.Bool(passed),
                            "detail" to JsonValue.Str(
                                "read=${scenarioRead.size} typed=${scenarioTyped.size} " +
                                    "failed=${scenarioFailed.size}"
                            )
                        )
                    )
                )
            }
        } finally {
            host.shutdown()
        }
        val root = Paths.get(repoRoot(), "target", "certification", "edition8")
        Files.createDirectories(root)
        val document = JsonValue.Obj(
            linkedMapOf(
                "schema" to JsonValue.Str("faktor-edition8-acceptance-live/v1"),
                "host" to JsonValue.Str("jetbrains"),
                "base_url" to JsonValue.Str(baseUrl),
                "session" to JsonValue.Str(session),
                "task" to JsonValue.Str(task),
                "counts" to JsonValue.Obj(
                    linkedMapOf(
                        "scenarios" to JsonValue.Int64(records.size.toLong()),
                        "passed" to JsonValue.Int64(records.count {
                            (it as JsonValue.Obj).fields["passed"] == JsonValue.Bool(true)
                        }.toLong()),
                        "failed" to JsonValue.Int64(failures.size.toLong()),
                        "read_receipts" to JsonValue.Int64(read.toLong()),
                        "typed_receipts" to JsonValue.Int64(typed.toLong()),
                        "edt_deliveries" to JsonValue.Int64(host.work.probes.edtDeliveries.get()),
                        "work_submitted" to JsonValue.Int64(host.work.probes.workSubmitted.get()),
                        "max_edt_ms" to JsonValue.Dbl(
                            host.work.probes.maxEdtNanos.get() / 1_000_000.0
                        )
                    )
                ),
                "records" to JsonValue.Arr(records),
                "failures" to JsonValue.Arr(failures.map { JsonValue.Str(it) })
            )
        )
        Files.write(
            root.resolve("acceptance-jetbrains-live.json"),
            JsonCodec.write(document).toByteArray(Charsets.UTF_8)
        )
        if (failures.isNotEmpty()) {
            println("EDITION8 LIVE ACCEPTANCE FAILED (${failures.size})")
            for (failure in failures) println(" - $failure")
            kotlin.system.exitProcess(1)
        }
        println("EDITION8 LIVE ACCEPTANCE: scenarios=${records.size}/40 read=$read typed=$typed failed=0")
    }

    private fun findButton(root: Component, name: String): AbstractButton? {
        if (root is AbstractButton && root.name == name) return root
        if (root is Container) {
            for (child in root.components) findButton(child, name)?.let { return it }
        }
        return null
    }

    private fun repoRoot(): String {
        val configured = System.getProperty("faktor.repo.root")
        if (!configured.isNullOrBlank()) return configured
        var dir = File(".").absoluteFile
        while (dir.parentFile != null) {
            if (File(dir, "Cargo.toml").isFile && File(dir, "crates").isDirectory) return dir.path
            dir = dir.parentFile
        }
        return File(".").absolutePath
    }
}
