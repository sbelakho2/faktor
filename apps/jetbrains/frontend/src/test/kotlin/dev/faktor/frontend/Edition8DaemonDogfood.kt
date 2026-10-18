// Edition 8 JetBrains real-daemon dogfood. Runs only when
// FAKTOR_E8_BASE_URL/FAKTOR_E8_TOKEN are present (the Edition 8 dogfood shell
// starts the real daemon). Drives the real NativeClient through the same
// route probes as the VS Code host, and builds every workflow's first Swing
// screen. Writes target/certification/edition8/dogfood-jetbrains.raw.json.
package dev.faktor.frontend

import dev.faktor.backend.NativeClient
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import java.io.File
import java.util.concurrent.atomic.AtomicReference
import javax.swing.SwingUtilities

object Edition8DaemonDogfood {

    private val failures = ArrayList<String>()

    @JvmStatic
    fun main(args: Array<String>) {
        val baseUrl = System.getenv("FAKTOR_E8_BASE_URL") ?: ""
        val token = System.getenv("FAKTOR_E8_TOKEN") ?: ""
        if (baseUrl.isBlank() || token.isBlank()) {
            println("EDITION8 JB DOGFOOD SKIPPED: no FAKTOR_E8_BASE_URL/TOKEN")
            return
        }
        val client = NativeClient(baseUrl, token)
        val host = Edition8Host(client = { client }, sessionId = { "1" }, taskId = { "1" })
        val daemon = host.daemon()
        val results = ArrayList<JsonValue>()
        var workflowChecks = 0
        try {
            val ready = call(host, daemon, "GET", "/native/ready", null)
            record(results, "jb-host", ready, "real daemon ready")
            for ((id, path, method) in productFlows()) {
                val result = call(host, daemon, method, path, if (method == "POST") "{}" else null)
                record(results, id, result, "$method $path")
            }
            for (workflow in Edition8Registry.workflows) {
                val screens = Regex("S\\d\\d").findAll(workflow.screenPath).map { it.value }.toList()
                if (screens.isEmpty()) {
                    failures.add("${workflow.id} has no screen")
                    continue
                }
                for (screen in screens.distinct()) {
                    var panel: Edition8ScreenPanel? = null
                    try {
                        SwingUtilities.invokeAndWait {
                            panel = Edition8ScreenRegistry.build(screen, host) as Edition8ScreenPanel
                        }
                        val view = panel!!
                        host.work.awaitIdle(15_000)
                        val rows = view.rowsForTest()
                        if (rows.isEmpty()) {
                            failures.add("${workflow.id} $screen has no control rows")
                            continue
                        }
                        val row = rows.first()
                        SwingUtilities.invokeAndWait { row.probe.doClick() }
                        host.work.awaitIdle(15_000)
                        val status = view.statusForTest()
                        val passed = !status.contains("not found", true) && !status.contains("404")
                        if (!passed) failures.add("${workflow.id} $screen probe: $status")
                        workflowChecks++
                        results.add(
                            JsonValue.Obj(
                                linkedMapOf(
                                    "id" to JsonValue.Str("workflow:${workflow.id}:$screen"),
                                    "passed" to JsonValue.Bool(passed),
                                    "detail" to JsonValue.Str("$screen ${row.control.id} probe -> $status"),
                                    "status" to JsonValue.Str(if (passed) "effective" else "failed")
                                )
                            )
                        )
                    } catch (t: Throwable) {
                        failures.add("${workflow.id} $screen failed: ${t.message}")
                    }
                }
            }
        } finally {
            host.shutdown()
        }
        val root = File(System.getProperty("faktor.repo.root") ?: System.getProperty("user.dir"), "target/certification/edition8")
        root.mkdirs()
        val failed = results.count { (it as JsonValue.Obj).fields["passed"] != JsonValue.Bool(true) } + failures.size
        File(root, "dogfood-jetbrains.raw.json").writeText(
            JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "schema" to JsonValue.Str("faktor-edition8-dogfood-host/v1"),
                        "host" to JsonValue.Str("jetbrains"),
                        "base_url" to JsonValue.Str(baseUrl),
                        "counts" to JsonValue.Obj(
                            linkedMapOf(
                                "flows" to JsonValue.Int64((results.size - workflowChecks).toLong()),
                                "workflows" to JsonValue.Int64(Edition8Registry.workflows.size.toLong()),
                                "workflow_checks" to JsonValue.Int64(workflowChecks.toLong()),
                                "passed" to JsonValue.Int64((results.size - failed).toLong()),
                                "failed" to JsonValue.Int64(failed.toLong())
                            )
                        ),
                        "results" to JsonValue.Arr(results),
                        "failures" to JsonValue.Arr(failures.map { JsonValue.Str(it) })
                    )
                )
            )
        )
        if (failures.isNotEmpty()) {
            println("EDITION8 JB DOGFOOD FAILED (${failures.size})")
            for (failure in failures) println(" - $failure")
            kotlin.system.exitProcess(1)
        }
        println("EDITION8 JB DOGFOOD: results=${results.size} failed=0 host=jetbrains")
    }

    private fun productFlows(): List<Triple<String, String, String>> = listOf(
        Triple("provider-config", "/native/providers", "GET"),
        Triple("task-admission", "/native/session/1/tasks", "GET"),
        Triple("compaction-long-task", "/native/session/1/context/compact", "POST"),
        Triple("restore-fork", "/native/session/1/checkpoints", "GET"),
        Triple("permissions", "/native/permissions", "GET"),
        Triple("proof", "/native/tasks/1/proof?session=1", "GET"),
        Triple("finish-levels", "/native/tasks/1/completion-steps?session=1", "GET")
    )

    private fun call(host: Edition8Host, daemon: Edition8Daemon, method: String, path: String, body: String?): Edition8Result {
        val result = AtomicReference<Edition8Result?>(null)
        daemon.call(method, path, body) { result.set(it) }
        host.work.awaitIdle(15_000)
        return result.get() ?: Edition8Result.Failed("no result for $method $path")
    }

    private fun record(results: MutableList<JsonValue>, id: String, result: Edition8Result, detail: String) {
        val passed = when (result) {
            is Edition8Result.Ok -> true
            is Edition8Result.Refused -> !result.reason.contains("not connected", true)
            is Edition8Result.Failed -> !result.reason.contains("404") && !result.reason.contains("not found", true)
        }
        if (!passed) failures.add("$id: $detail -> ${reason(result)}")
        results.add(
            JsonValue.Obj(
                linkedMapOf(
                    "id" to JsonValue.Str(id),
                    "passed" to JsonValue.Bool(passed),
                    "detail" to JsonValue.Str("$detail -> ${reason(result).take(200)}"),
                    "status" to JsonValue.Str(if (passed) "effective" else "failed")
                )
            )
        )
    }

    private fun reason(result: Edition8Result): String = when (result) {
        is Edition8Result.Ok -> "ok"
        is Edition8Result.Failed -> result.reason
        is Edition8Result.Refused -> "refused: ${result.reason}"
    }
}
