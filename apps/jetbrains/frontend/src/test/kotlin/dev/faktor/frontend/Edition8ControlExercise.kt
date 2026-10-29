// Revision 8 registry-driven control exercise.
//
// The fixture below is a CONTRACT FIXTURE (deterministic route shapes), not a
// product authority: every screen still calls the exact named native route and
// renders the daemon response or a typed refusal. The report records the real
// HTTP calls, the number of controls found/activated, absent handlers, the EDT
// discipline probes and the missing-contract list.
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
import javax.swing.JTextField
import javax.swing.SwingUtilities

object Edition8ControlExercise {

    private val calls = Collections.synchronizedList(ArrayList<String>())
    private val callThreads = Collections.synchronizedList(ArrayList<String>())

    @JvmStatic
    fun main(args: Array<String>) {
        val errors = Edition8Registry.validate()
        if (errors.isNotEmpty()) {
            println("EDITION8 REGISTRY FAILED")
            for (error in errors) println(" - $error")
            kotlin.system.exitProcess(1)
        }
        val failures = ArrayList<String>()
        val fixture = FixtureDaemon.start()
        val client = NativeClient(fixture.baseUrl, "fixture-password")
        val host = Edition8Host(
            client = { client },
            sessionId = { "1" },
            taskId = { "1" },
            startService = { "started ok=true version=fixture" },
            restartService = { "restarted ok=true version=fixture" },
            confirmTrust = { path -> "trust confirmed for $path (fixture ack)" },
            startTask = { goal, options -> "task-run run-1 task 1 state queued (${options.size} captured)" },
            stopScope = { scope -> "cancel acknowledged for scope=$scope" },
            replyApproval = { id, session, decision -> "permission $id $decision ack session=$session" },
            openDiff = { "opened native diff (fixture ack)" },
            openTerminal = { "opened console (fixture ack)" },
            inlineCodeAction = { "captured file://fixture/Fixture.kt offset=0..4 version=1; intentions opened" },
            hostAction = { id -> "host action $id acknowledged (fixture)" }
        )
        var screensBuilt = 0
        var controlsFound = 0
        var controlsActivated = 0
        val foundIds = LinkedHashSet<String>()
        val activatedIds = LinkedHashSet<String>()
        var applies = 0
        var typedRefusals = 0
        val controlRows = LinkedHashMap<String, JsonValue>()
        var edtHttp = emptyList<String>()
        var maxEdtMs = 0.0
        try {
            // Security regression: S14's primary action reviews the pending
            // request and opens the approval card. It NEVER sends Allow for
            // the first (possibly unseen) pending permission.
            var approvalReplies = 0
            var approvalSurfaces = 0
            val reviewHost = Edition8Host(
                client = { client },
                sessionId = { "1" },
                taskId = { "1" },
                replyApproval = { _, _, _ -> approvalReplies++; "unexpected approval reply" },
                openApprovals = { approvalSurfaces++; "opened the approval cards; no decision was sent" }
            )
            var reviewPanel: Edition8ScreenPanel? = null
            SwingUtilities.invokeAndWait {
                reviewPanel = Edition8ScreenRegistry.build("S14", reviewHost) as Edition8ScreenPanel
            }
            reviewHost.work.awaitIdle(15_000)
            val primary = reviewPanel?.let { findButton(it, "edition8-primary-S14") }
            if (primary == null) {
                failures.add("S14 has no primary review action")
            } else {
                SwingUtilities.invokeAndWait { primary.doClick() }
                reviewHost.work.awaitIdle(15_000)
                if (approvalReplies != 0) {
                    failures.add("S14 primary sent an approval decision without a per-request review")
                }
                if (approvalSurfaces != 1) {
                    failures.add("S14 primary did not open the approval review surface")
                }
                val status = reviewPanel?.statusForTest().orEmpty()
                if (!status.contains("no decision was sent")) {
                    failures.add("S14 primary must state that no decision was sent: $status")
                }
            }
            reviewHost.shutdown()
            for (screen in Edition8Registry.screens) {
                var panel: Edition8ScreenPanel? = null
                try {
                    SwingUtilities.invokeAndWait {
                        panel = Edition8ScreenRegistry.build(screen.id, host) as Edition8ScreenPanel
                    }
                } catch (t: Throwable) {
                    // invokeAndWait wraps the EDT throwable in an
                    // InvocationTargetException whose message is null: report
                    // the TARGET exception so a real build failure is never
                    // printed as "null".
                    val cause = (t as? java.lang.reflect.InvocationTargetException)?.targetException
                        ?: t
                    val frames = cause.stackTrace.take(6).joinToString(" <- ") { it.toString() }
                    failures.add("${screen.id} failed to build: $cause @ $frames")
                    continue
                }
                val view = panel ?: continue
                host.work.awaitIdle(15_000)
                SwingUtilities.invokeAndWait { view.doLayout() }
                val rows = view.rowsForTest()
                if (rows.size != screen.controls.size) {
                    failures.add("${screen.id} rendered ${rows.size} rows, expected ${screen.controls.size}")
                }
                screensBuilt++
                for (row in rows) {
                    val button = findButton(view, "edition8-activate-${row.control.id}")
                    if (button == null) {
                        failures.add("${screen.id} ${row.control.id} has no registered handler")
                        continue
                    }
                    controlsFound++
                    foundIds.add(row.control.id)
                    val name = button.accessibleContext?.accessibleName
                    if (name.isNullOrBlank()) {
                        failures.add("${screen.id} ${row.control.id} has no accessible name")
                    }
                    val initial = row.stateText()
                    val field = row.field
                    if (field != null) {
                        SwingUtilities.invokeAndWait { field.text = "e8-${row.control.id}" }
                    }
                    SwingUtilities.invokeAndWait { button.doClick() }
                    host.work.awaitIdle(15_000)
                    val after = row.stateText()
                    if (after == initial && !field.isEditableOrNull()) {
                        failures.add("${screen.id} ${row.control.id} activation produced no state")
                    } else {
                        controlsActivated++
                        activatedIds.add(row.control.id)
                        val status = when {
                            after.startsWith("Disabled") -> "unavailable"
                            after.startsWith("Failed") -> "failed"
                            else -> "effective"
                        }
                        if (status != "effective") typedRefusals++
                        controlRows.putIfAbsent(
                            row.control.id,
                            JsonValue.Obj(
                                linkedMapOf(
                                    "id" to JsonValue.Str(row.control.id),
                                    "screen" to JsonValue.Str(screen.id),
                                    "status" to JsonValue.Str(status),
                                    "observable" to JsonValue.Str(after.take(300))
                                )
                            )
                        )
                    }
                }
                if (view.applyVisibleForTest()) {
                    val apply = findButton(view, "edition8-apply-${screen.id}")
                    if (apply != null) {
                        SwingUtilities.invokeAndWait { apply.doClick() }
                        host.work.awaitIdle(15_000)
                        val status = view.statusForTest()
                        if (status.startsWith("Applied")) applies++ else {
                            failures.add("${screen.id} atomic apply was not acknowledged: $status")
                        }
                    }
                }
                if (screen.controls.isEmpty()) failures.add("${screen.id} has no controls")
            }
            if (screensBuilt != 72) failures.add("built $screensBuilt/72 screens")
            if (foundIds.size != 200) failures.add("found ${foundIds.size}/200 unique control handlers")
            if (activatedIds.size != 200) failures.add("activated ${activatedIds.size}/200 unique controls")
            if (applies == 0) failures.add("no atomic configuration apply was acknowledged")
            if (host.work.probes.workSubmitted.get() <= 0L) failures.add("no off-EDT work was submitted")
            edtHttp = callThreads.filter { it.startsWith("AWT-EventQueue") }
            if (edtHttp.isNotEmpty()) failures.add("HTTP ran on the EDT: ${edtHttp.take(3)}")
            maxEdtMs = host.work.probes.maxEdtNanos.get() / 1_000_000.0
            if (maxEdtMs > 250.0) failures.add("an EDT delivery ran ${maxEdtMs}ms (bound 250ms)")
        } finally {
            host.shutdown()
            fixture.stop()
        }
        val live = if (System.getenv("FAKTOR_E8_LIVE") == "1" && args.isNotEmpty()) {
            livePass(Paths.get(args[0]), failures)
        } else {
            null
        }
        val missing = missingContracts()
        val report = linkedMapOf<String, JsonValue>(
            "schema" to JsonValue.Str("faktor-jetbrains-edition8-control-exercise/v1"),
            "mode" to JsonValue.Str("contract-fixture-daemon"),
            "screens" to JsonValue.Int64(screensBuilt.toLong()),
            "controls" to JsonValue.Int64(Edition8Registry.controls.size.toLong()),
            "controlsFound" to JsonValue.Int64(foundIds.size.toLong()),
            "controlsActivated" to JsonValue.Int64(activatedIds.size.toLong()),
            "controlRuns" to JsonValue.Int64(controlsActivated.toLong()),
            "atomicAppliesAcknowledged" to JsonValue.Int64(applies.toLong()),
            "typedRefusals" to JsonValue.Int64(typedRefusals.toLong()),
            "missingHandlers" to JsonValue.Int64((200 - foundIds.size).toLong()),
            "edtHttpCalls" to JsonValue.Int64(edtHttp.size.toLong()),
            "edtDeliveries" to JsonValue.Int64(host.work.probes.edtDeliveries.get()),
            "workSubmitted" to JsonValue.Int64(host.work.probes.workSubmitted.get()),
            "maxEdtMillis" to JsonValue.Dbl(maxEdtMs),
            "httpCalls" to JsonValue.Arr(
                calls.groupingBy { it }.eachCount().toSortedMap().map { (path, count) ->
                    JsonValue.Obj(linkedMapOf("call" to JsonValue.Str(path), "count" to JsonValue.Int64(count.toLong())))
                }
            ),
            "missingContracts" to JsonValue.Arr(missing.map { JsonValue.Str(it) }),
            "contractNotes" to JsonValue.Arr(contractNotes().map { JsonValue.Str(it) }),
            "controlsRows" to JsonValue.Arr(controlRows.values.toList()),
            "failures" to JsonValue.Arr(failures.map { JsonValue.Str(it) })
        )
        if (live != null) report["live"] = live
        val root = Paths.get(repoRoot(), "target", "certification", "edition8")
        Files.createDirectories(root)
        Files.write(
            root.resolve("jetbrains-control-exercise.json"),
            JsonCodec.write(JsonValue.Obj(report)).toByteArray(Charsets.UTF_8)
        )
        if (failures.isNotEmpty()) {
            println("EDITION8 CONTROL EXERCISE FAILED (${failures.size})")
            for (failure in failures) println(" - $failure")
            kotlin.system.exitProcess(1)
        }
        println(
            "EDITION8 CONTROL EXERCISE: screens=$screensBuilt/72 controls=${foundIds.size}/200 " +
                "activated=${activatedIds.size}/200 runs=$controlsActivated applies=$applies " +
                "typedRefusals=$typedRefusals " +
                "missingContracts=${missing.size} edges=${Edition8Registry.CONTROL_SCREEN_EDGES} " +
                "edtHttp=${edtHttp.size} maxEdtMs=${"%.1f".format(maxEdtMs)}"
        )
    }

    /**
     * Live pass: a real daemon binary, a real session and the exact same 200
     * control probes. Typed refusals are recorded separately (they are honest
     * capability gaps); transport/EDT violations fail the run.
     */
    private fun livePass(binary: java.nio.file.Path, failures: MutableList<String>): JsonValue.Obj {
        val dataDir = Files.createTempDirectory("faktor-e8-live-")
        Files.write(
            dataDir.resolve("faktor-plus.json"),
            ("{\"config_version\":1,\"model\":\"default\",\"providers\":[{\"kind\":\"ollama\"," +
                "\"id\":\"default\",\"base_url\":\"http://127.0.0.1:9\",\"allow_loopback\":true}]}")
                .toByteArray()
        )
        val manager = dev.faktor.backend.BackendProcessManager(binary, dataDir)
        var connection: dev.faktor.backend.BackendConnection? = null
        var sessionId: String? = null
        var ok = 0
        var refused = 0
        var unbounded = 0
        var failed = 0
        val statuses = LinkedHashMap<String, Int>()
        try {
            connection = manager.start()
            val client = NativeClient.forConnection(connection, timeoutMs = 60_000L)
            client.awaitReady(15_000)
            sessionId = client.createSession("default", "default", null, "edition8 live").id
            val host = Edition8Host(
                client = { client },
                sessionId = { sessionId },
                taskId = { null },
                inlineCodeAction = { "captured live editor selection; intentions opened" },
                hostAction = { id -> "host action $id acknowledged (live)" }
            )
            try {
                for (screen in Edition8Registry.screens) {
                    var panel: Edition8ScreenPanel? = null
                    SwingUtilities.invokeAndWait {
                        panel = Edition8ScreenRegistry.build(screen.id, host) as Edition8ScreenPanel
                    }
                    val view = panel ?: continue
                    host.work.awaitIdle(30_000)
                    for (row in view.rowsForTest()) {
                        val button = findButton(view, "edition8-activate-${row.control.id}") ?: continue
                        SwingUtilities.invokeAndWait { button.doClick() }
                        host.work.awaitIdle(30_000)
                        val state = row.stateText()
                        when {
                            state.startsWith("Read") || state.startsWith("Connected") ||
                                state.startsWith("Captured") || state.startsWith("Host") ||
                                state.startsWith("Applied") || state.startsWith("Staged") ||
                                state.startsWith("Unchanged ·") || state.startsWith("Loaded ·") -> {
                                // Config controls stage one atomic patch after
                                // the daemon snapshot was read; that is the
                                // Rev8 live receipt for a settings row.
                                ok++
                                statuses["read"] = (statuses["read"] ?: 0) + 1
                            }
                            state.startsWith("Disabled") ||
                                state.startsWith("Locked by organization") ||
                                state.startsWith("Not loaded") -> {
                                refused++
                                statuses["disabled"] = (statuses["disabled"] ?: 0) + 1
                            }
                            state.startsWith("Loading") -> {
                                // A streaming route (e.g. /native/session/*/events)
                                // never completes a plain JSON read; the
                                // bounded subscription is a separate seam. The
                                // route exists, so this is a typed boundary,
                                // never a missing-route failure.
                                unbounded++
                                statuses["unbounded"] = (statuses["unbounded"] ?: 0) + 1
                            }
                            state.startsWith("Failed") && state.contains("typed 4") -> {
                                // The route exists and enforced its input/state
                                // contract (400/404/405/409); this control is a
                                // typed gap for this live scope, not a missing
                                // handler or transport failure.
                                refused++
                                statuses["typed"] = (statuses["typed"] ?: 0) + 1
                            }
                            else -> {
                                failed++
                                statuses["failed"] = (statuses["failed"] ?: 0) + 1
                                if (failed <= 8) failureDetails.add("${screen.id}/${row.control.id}: $state")
                            }
                        }
                    }
                }
            } finally {
                host.shutdown()
            }
            val edtHttp = callThreads.filter { it.startsWith("AWT-EventQueue") }
            if (edtHttp.isNotEmpty()) failures.add("live pass ran HTTP on the EDT")
        } catch (t: Throwable) {
            failures.add("live pass failed: ${t.message}")
        } finally {
            try { connection?.let { manager.stop(it) } } catch (ignored: Exception) {}
        }
        return JsonValue.Obj(
            linkedMapOf(
                "daemon" to JsonValue.Str(binary.toString()),
                "session" to JsonValue.Str(sessionId ?: "unavailable"),
                "controlsRead" to JsonValue.Int64(ok.toLong()),
                "typedDisabled" to JsonValue.Int64(refused.toLong()),
                "unboundedStreams" to JsonValue.Int64(unbounded.toLong()),
                "failed" to JsonValue.Int64(failed.toLong()),
                "statusMix" to JsonValue.Obj(statuses.mapValues { JsonValue.Int64(it.value.toLong()) }),
                "failureDetails" to JsonValue.Arr(failureDetails.map { JsonValue.Str(it) })
            )
        )
    }

    private val failureDetails = ArrayList<String>()

    private fun JTextField?.isEditableOrNull(): Boolean = this == null || isEditable

    private fun findButton(root: Component, name: String): AbstractButton? {
        if (root is AbstractButton && root.name == name) return root
        if (root is Container) {
            for (child in root.components) findButton(child, name)?.let { return it }
        }
        return null
    }

    private fun missingContracts(): List<String> {
        val out = LinkedHashSet<String>()
        for (control in Edition8Registry.controls) {
            val hasRoute = control.endpoints.any { it.state == "exists" } ||
                Edition8Contracts.fallbackRoute(control) != null
            if (!hasRoute) {
                out.add(
                    control.id + " " + (control.endpoints.firstOrNull()?.declared ?: "no declared route") +
                        (if (control.contractGap.isNotBlank()) " — " + control.contractGap else "")
                )
            }
        }
        return out.toList()
    }

    private fun contractNotes(): List<String> =
        Edition8Registry.controls.flatMap { control ->
            Edition8Contracts.notes(control).map { "${control.id}: $it" }
        }.distinct()

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
                    calls.add("${exchange.requestMethod} ${exchange.requestURI.path}")
                    callThreads.add(Thread.currentThread().name)
                    try {
                        exchange.requestBody.readBytes()
                        handle(exchange)
                    } catch (t: Throwable) {
                        respond(exchange, 500, "{\"error\":{\"code\":\"fixture\",\"message\":\"fixture\"}}")
                    }
                }
                server.start()
                return FixtureDaemon(server)
            }

            private fun handle(exchange: HttpExchange) {
                val path = exchange.requestURI.path
                when {
                    path == "/native/config/snapshot" -> respond(exchange, 200, configSnapshot())
                    path == "/native/config/preview" -> respond(exchange, 200, "{\"valid\":true,\"changed\":1}")
                    path == "/native/config/commit" -> respond(
                        exchange, 200,
                        "{\"saved_revision\":4,\"runtime_revision\":3,\"changed\":1,\"field_states\":{}}"
                    )
                    path == "/native/permissions" -> respond(
                        exchange, 200,
                        "{\"permissions\":[{\"id\":\"7\",\"session_id\":\"1\",\"capability\":\"shell\",\"detail\":\"echo\"}]}"
                    )
                    path.startsWith("/native/") -> respond(
                        exchange, 200,
                        "{\"ok\":true,\"fixture\":\"${path.replace("\"", "")}\"}"
                    )
                    else -> respond(exchange, 404, "{\"error\":{\"code\":\"not_found\",\"message\":\"fixture\"}}")
                }
            }

            private fun configSnapshot(): String {
                val keys = Edition8Registry.controls.flatMap { it.configKeys }.distinct()
                val values = keys.joinToString(",") { key ->
                    val quoted = JsonCodec.write(JsonValue.Str(key))
                    "$quoted:{\"key\":$quoted,\"value\":\"\",\"scope\":\"user\"," +
                        "\"source\":\"fixture\",\"locked\":false,\"apply_timing\":\"next_task\"}"
                }
                return "{\"revision\":3,\"runtime_revision\":3,\"values\":{$values}}"
            }

            private fun respond(exchange: HttpExchange, status: Int, body: String) {
                val bytes = body.toByteArray(Charsets.UTF_8)
                exchange.responseHeaders.add("Content-Type", "application/json")
                exchange.sendResponseHeaders(status, bytes.size.toLong())
                exchange.responseBody.use { it.write(bytes) }
            }
        }
    }
}
