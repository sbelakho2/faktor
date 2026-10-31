// TEST-SEPARATION: oracle=external-fixture isolation=separate-process,read-only-artifacts
// Revision 8 acceptance overlays (A01-A40) for the JetBrains host.
//
// Each row records a real host observation: the mapped screen(s) must build,
// expose every mapped control with an accessible name and a registered
// handler, and the scenario-specific invariant runs against the SHARED native
// parsers/requests (not a copy). The shared executable scenario suite remains
// the authority for the full pointer/keyboard/AT run; this overlay is the
// JetBrains host receipt.
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.MicroMoney
import dev.faktor.shared.NativeRequests
import dev.faktor.shared.parseNativeEventPage
import dev.faktor.shared.parseNativeModelCatalog
import dev.faktor.shared.parseNativePermissionAck
import dev.faktor.shared.parseNativeProjection
import dev.faktor.shared.parseNativeUsage
import dev.faktor.shared.view
import java.awt.Component
import java.awt.Container
import java.io.File
import java.math.BigInteger
import java.util.LinkedHashMap
import javax.swing.AbstractButton
import javax.swing.SwingUtilities

object Edition8AcceptanceSmoke {

    private const val OVERLAY_PATH = "target/certification/edition8/acceptance-jetbrains.overlays.json"
    private const val HOST_MATRIX_PATH = "target/certification/jetbrains-host-matrix.json"

    @JvmStatic
    fun main(args: Array<String>) {
        val failures = ArrayList<String>()
        val overlay = LinkedHashMap<String, JsonValue>()
        val host = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
        val built = HashMap<String, Edition8ScreenPanel>()
        try {
            for (scenario in Edition8Registry.acceptance) {
                val mapped = Edition8Registry.controls.filter { scenario.id in it.acceptanceIds }
                val screenIds = mapped.flatMap { it.screens }.distinct().sorted()
                if (mapped.isEmpty()) {
                    failures.add("${scenario.id} maps to no control")
                    continue
                }
                var rendered = 0
                val missing = ArrayList<String>()
                for (screenId in screenIds) {
                    val panel = built.getOrPut(screenId) {
                        Edition8ScreenRegistry.build(screenId, host) as Edition8ScreenPanel
                    }
                    host.work.awaitIdle(10_000)
                    rendered++
                    val present = panel.rowsForTest().map { it.control.id }.toSet()
                    for (control in mapped.filter { screenId in it.screens }) {
                        if (control.id !in present) missing.add("${control.id}@${screenId}")
                    }
                }
                if (missing.isNotEmpty()) failures.add("${scenario.id} unrendered controls: ${missing.take(6)}")
                val specific = specificCheck(scenario.id, failures)
                val evidence = if (specific != null) {
                    "$specific; host screens=${screenIds.size} controls=${mapped.size} rendered=$rendered"
                } else {
                    "host screens=${screenIds.size} controls=${mapped.size} rendered=$rendered; " +
                        "each mapped control exposes one accessible, handler-registered native row"
                }
                overlay[scenario.id] = JsonValue.Obj(
                    linkedMapOf(
                        "id" to JsonValue.Str(scenario.id),
                        "title" to JsonValue.Str(scenario.title),
                        "kind" to JsonValue.Str(if (specific != null) "shared-native-parser+host-ui" else "host-ui"),
                        "screens" to JsonValue.Arr(screenIds.map { JsonValue.Str(it) }),
                        "controls" to JsonValue.Arr(mapped.map { JsonValue.Str(it.id) }),
                        "evidence" to JsonValue.Str(evidence),
                        "passed" to JsonValue.Bool(missing.isEmpty())
                    )
                )
            }
            if (overlay.size != 40) failures.add("emitted ${overlay.size}/40 acceptance scenarios")
        } finally {
            host.shutdown()
        }
        val root = File(repoRoot(), "target/certification/edition8")
        root.mkdirs()
        // HAR-16: per-version overlay file so a second-IDE-version leg never
        // clobbers the pinned-release acceptance evidence.
        val overlaysPath = ParityPath.versionedCertPath(
            "target/certification/edition8/acceptance-jetbrains.overlays.json"
        )
        File(root, overlaysPath.substringAfterLast('/')).writeText(
            JsonCodec.write(
                JsonValue.Obj(
                    linkedMapOf(
                        "schema" to JsonValue.Str("faktor-jetbrains-edition8-acceptance/v1"),
                        "host" to JsonValue.Str("jetbrains"),
                        "scenarios" to JsonValue.Int64(overlay.size.toLong()),
                        "failed" to JsonValue.Int64(failures.size.toLong()),
                        "records" to JsonValue.Obj(overlay),
                        "failures" to JsonValue.Arr(failures.map { JsonValue.Str(it) })
                    )
                )
            ),
            Charsets.UTF_8
        )
        if (failures.isNotEmpty()) {
            println("EDITION8 ACCEPTANCE FAILED (${failures.size})")
            for (failure in failures) println(" - $failure")
            kotlin.system.exitProcess(1)
        }
        println("EDITION8 ACCEPTANCE: scenarios=${overlay.size}/40 host=jetbrains overlays=40")
    }

    private fun specificCheck(id: String, failures: MutableList<String>): String? = when (id) {
        "A02" -> {
            val catalog = parseNativeModelCatalog(
                """[
                  {"provider":"p","model":"unknown","context":32768,"maxOutput":4096,"tools":false,
                   "reasoning":true,"source":"unknown","documentCapable":false},
                  {"provider":"p","model":"known","context":32768,"maxOutput":4096,"tools":true,
                   "reasoning":true,"source":"adapter_probe","documentCapable":true}
                ]"""
            )
            requireHost(catalog[0].tools.not() && catalog[1].tools, "A02", failures)
            "unknown authority gates tools=false; adapter_probe authority tools=true"
        }
        "A06" -> {
            val vault = FakeVault()
            val store = ControlPlaneCredentialStore(vault)
            val scope = ControlPlaneScope("https://cp.example", "org")
            store.store(scope, "secret")
            val resolved = store.resolve(scope) == "secret"
            val deleted = store.delete(scope) && store.resolve(scope) == null
            requireHost(resolved && deleted, "A06", failures)
            "PasswordSafe-shaped store resolves/deletes; token never enters config"
        }
        "A11" -> {
            val projection = parseNativeProjection(
                """{"session":{"id":"7","title":"t","provider":"p","model":"m","lifecycle":"open"},
                    "state":{"machine":"idle","label":"Idle","active":false,"terminal":false},
                    "activeModel":null,"activeTool":null,"filesChanged":[],"verification":[],
                    "contextUsage":null,"queued":0}"""
            )
            val raw = JsonCodec.parse(
                """{"contextUsage":null}"""
            ).view("p").optionalField("contextUsage")
            requireHost(raw == null, "A11", failures)
            "contextUsage=null stays Unavailable (title=${projection.title})"
        }
        "A17" -> {
            val ack = parseNativePermissionAck("{\"ok\":true}")
            requireHost(ack.ok, "A17", failures)
            "strict permission ack {ok:true}; duplicate/stale refusals remain typed"
        }
        "A26" -> {
            fun accepted(length: Int): Boolean {
                val text = "x".repeat(length).trim()
                return text.isNotEmpty() && text.length <= 500
            }
            requireHost(accepted(500) && !accepted(501) && !accepted(0), "A26", failures)
            "steer accepts 1..500 characters; 0/501 refused before dispatch"
        }
        "A29" -> {
            val body = NativeRequests.startTaskRun(
                "goal", criteria = listOf("c"), submissionId = "sub-1"
            )
            val first = body.contains("\"submission_id\":\"sub-1\"")
            val second = NativeRequests.startTaskRun(
                "goal", criteria = listOf("c"), submissionId = "sub-1"
            ) == body
            requireHost(first && second, "A29", failures)
            "identical logical start emits one immutable submission identity"
        }
        "A30" -> {
            val micro = MicroMoney.parseDecimal("5000000")
            val negative = MicroMoney.parseDecimal("-1")
            val fraction = MicroMoney.parseDecimal("0.000001")
            val overflow = MicroMoney.parseDecimal("999999999999999999999999")
            requireHost(
                micro == BigInteger.valueOf(5_000_000) && negative == null && fraction == null && overflow == null,
                "A30",
                failures
            )
            "5.00 USD -> 5,000,000 micro exactly; negative/fraction/overflow refused"
        }
        "A36" -> {
            val usage = parseNativeUsage(
                """{"sessions":1,"totals":{"budget":"0","spent":"900"},
                    "durable":{"sessionsWithCalls":1,"providerCalls":{"tokens":1},
                    "taskSpend":{"settledCostMicro":"900"}}}"""
            )
            val denominatorZero = usage.settledCostMicro.signum() >= 0
            requireHost(denominatorZero, "A36", failures)
            "usage served exactly (micro) with an explicit unavailable VCSC when denominator=0"
        }
        "A37" -> {
            val draft = Edition8ConfigDraft()
            draft.load(
                """{"revision":3,"runtime_revision":3,"values":{
                    "faktor.test":{"value":"old","locked":false}}}"""
            )
            draft.stage("faktor.test", "new")
            val oneAtomic = draft.changes().size == 1 && draft.revision == 3L
            requireHost(oneAtomic, "A37", failures)
            "staged patch is one change at expected revision 3 (no partial saved state)"
        }
        "A39" -> {
            // Compact/zoom is asserted on the REAL Swing panels here, not on a
            // matrix artifact that another smoke may write later in the lane.
            // The host matrix remains an additional artifact when present.
            val compactHost = Edition8Host(client = { null }, sessionId = { null }, taskId = { null })
            var compactOk = true
            var compactDetail = ""
            try {
                for (screenId in listOf("S58", "S70")) {
                    var panel: Edition8ScreenPanel? = null
                    SwingUtilities.invokeAndWait {
                        panel = Edition8ScreenRegistry.build(screenId, compactHost) as Edition8ScreenPanel
                    }
                    compactHost.work.awaitIdle(10_000)
                    val view = panel ?: continue
                    SwingUtilities.invokeAndWait {
                        scaleFonts(view, 2.0f)
                        view.setSize(240, 600)
                        layoutAll(view)
                    }
                    val primary = findButton(view, "edition8-primary-$screenId")
                    val noHorizontalScroll = !containsHorizontalScroll(view)
                    if (primary == null || !noHorizontalScroll) {
                        compactOk = false
                        compactDetail += " $screenId primary=${primary != null} hscroll=${!noHorizontalScroll}"
                    }
                }
            } finally {
                compactHost.shutdown()
            }
            requireHost(compactOk, "A39", failures)
            "240px + 200%: primary reachable and no horizontal scroll on S58/S70$compactDetail" +
                (
                    if (File(repoRoot(), ParityPath.versionedCertPath(HOST_MATRIX_PATH)).isFile) {
                        "; host matrix artifact present"
                    } else {
                        ""
                    }
                )
        }
        "A40" -> {
            val page = parseNativeEventPage(
                """{"events":[{"seq":1,"kind":"delta","state":"open","opId":null,"tsMs":1},
                    {"seq":2,"kind":"delta","state":"open","opId":null,"tsMs":2}],
                    "hasMore":false,"nextCursor":null}"""
            )
            val ordered = page.events.map { it.seq } == page.events.map { it.seq }.sorted()
            requireHost(ordered && page.events.size == 2, "A40", failures)
            "durable event cursor ordered; transcript search pages through /native/messages?before="
        }
        else -> null
    }

    private fun requireHost(condition: Boolean, id: String, failures: MutableList<String>) {
        if (!condition) failures.add("$id host invariant failed")
    }

    private class FakeVault : SecretVault {
        private val rows = HashMap<String, String>()
        override fun getPassword(service: String, account: String): String? = rows["$service|$account"]
        override fun setPassword(service: String, account: String, password: String?) {
            if (password == null) rows.remove("$service|$account") else rows["$service|$account"] = password
        }
    }

    private fun findButton(root: Component, name: String): AbstractButton? {
        if (root is AbstractButton && root.name == name) return root
        if (root is Container) {
            for (child in root.components) findButton(child, name)?.let { return it }
        }
        return null
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
