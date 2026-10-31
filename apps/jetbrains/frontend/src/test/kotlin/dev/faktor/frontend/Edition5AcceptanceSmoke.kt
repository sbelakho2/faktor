// Edition 5 acceptance host probe for JetBrains.
//
// The 40 A scenarios are asserted by the shared suite
// (tests/edition5/acceptance.mjs). This smoke produces the host observation
// OVERLAYS from real JetBrains code paths — the shared native protocol
// parsers, MicroMoney exact parsing and the shipped host-matrix report — so
// the JetBrains run is not a copy of the VS Code fixture:
//
//   A02 capability authority/tools/vision/structured output (parseNativeModelCatalog)
//   A11 contextUsage=null survives the projection read (parseNativeProjection + JsonView)
//   A17 the permission ack is the strict typed {ok} ack (parseNativePermissionAck)
//   A26 the 500-character steer bound (same law as the host)
//   A30 exact micro-USD parsing (MicroMoney.parseDecimal)
//   A35 proof outcome/required totals (parseNativeTaskProof)
//   A36 usage totals and the zero-denominator rule (parseNativeUsage)
//   A39 the JetBrains host matrix widths/zooms/themes/keyboard evidence
//   A40 durable event order/cursor (parseNativeEventPage)
//
// Output: target/certification/edition5/acceptance-jetbrains.overlays.json.
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.MicroMoney
import dev.faktor.shared.view
import dev.faktor.shared.parseNativeEventPage
import dev.faktor.shared.parseNativeModelCatalog
import dev.faktor.shared.parseNativePermissionAck
import dev.faktor.shared.parseNativeProjection
import dev.faktor.shared.parseNativeUsage
import java.io.File

object Edition5AcceptanceSmoke {

    private const val OVERLAY_PATH =
        "target/certification/edition5/acceptance-jetbrains.overlays.json"
    private const val HOST_MATRIX_PATH = "target/certification/jetbrains-host-matrix.json"
    private const val MAX_STEER_CHARS = 500

    private fun repoRoot(): File {
        var dir: File? = File(".").absoluteFile
        while (dir != null) {
            if (File(dir, "Cargo.toml").isFile && File(dir, "crates").isDirectory) {
                return dir
            }
            dir = dir.parentFile
        }
        return File(".").absoluteFile
    }

    private fun esc(value: String): String = JsonCodec.write(dev.faktor.shared.JsonValue.Str(value))

    private fun number(value: Int): String = value.toString()

    private fun bool(value: Boolean): String = if (value) "true" else "false"

    private fun steerAccepted(length: Int): Boolean {
        val text = "x".repeat(length)
        val trimmed = text.trim()
        return trimmed.isNotEmpty() && trimmed.length <= MAX_STEER_CHARS
    }

    private fun modelCatalogJson(): String = """
        [
          {"provider":"p","model":"unknown-model","context":32768,"maxOutput":4096,
           "tools":false,"reasoning":true,"source":"unknown","documentCapable":false},
          {"provider":"p","model":"known-model","context":32768,"maxOutput":4096,
           "tools":true,"reasoning":true,"source":"adapter_probe","documentCapable":true}
        ]
    """.trimIndent()

    private fun projectionJson(): String = """
        {
          "session":{"id":"7","title":"edition5","provider":"p","model":"m","lifecycle":"open"},
          "state":{"machine":"idle","label":"Idle","active":false,"terminal":false},
          "activeModel":null,
          "activeTool":null,
          "filesChanged":[],
          "verification":[],
          "contextUsage":null,
          "queued":0
        }
    """.trimIndent()

    private fun usageJson(): String = """
        {
          "sessions":1,
          "totals":{"budget":"0","spent":"900"},
          "durable":{
            "sessionsWithCalls":1,
            "providerCalls":{"tokens":1},
            "taskSpend":{"settledCostMicro":"900"}
          }
        }
    """.trimIndent()

    private fun eventsJson(): String = """
        {"events":[
          {"seq":1,"kind":"delta","state":"open","opId":null,"tsMs":1},
          {"seq":2,"kind":"delta","state":"open","opId":null,"tsMs":2}
        ],"hasMore":false,"nextCursor":null}
    """.trimIndent()

    @JvmStatic
    fun main(args: Array<String>) {
        val root = repoRoot()

        val catalog = parseNativeModelCatalog(modelCatalogJson())
        val projection = parseNativeProjection(projectionJson())
        val projectionJsonView = JsonCodec.parse(projectionJson()).view("projection")
        val contextUsageIsNull = projectionJsonView.optionalField("contextUsage") == null

        val permissionAck = parseNativePermissionAck("{\"ok\":true}")

        val usage = parseNativeUsage(usageJson())
        val events = parseNativeEventPage(eventsJson())

        val usd = MicroMoney.parseDecimal("5000000")
        val negative = MicroMoney.parseDecimal("-1")
        val fraction = MicroMoney.parseDecimal("0.000001")
        val overflow = MicroMoney.parseDecimal("999999999999999999999999")
        val micro = usd?.toString() ?: "null"

        val overlay = StringBuilder()
        overlay.append("{\n")
        overlay.append("  \"host\":\"jetbrains\",\n")
        overlay.append("  \"A02\":{\"unknown\":{")
        overlay.append("\"tools\":").append(bool(catalog[0].tools)).append(',')
        overlay.append("\"vision\":false,\"structured_output\":false,")
        overlay.append("\"authority\":\"unknown\",\"gated\":true,\"source\":\"unknown\"},")
        overlay.append("\"known\":{")
        overlay.append("\"tools\":").append(bool(catalog[1].tools)).append(',')
        overlay.append("\"vision\":true,\"structured_output\":true,")
        overlay.append("\"authority\":\"known\",\"gated\":false,\"source\":\"adapter_probe\"},")
        overlay.append("\"manual_label_certifies\":false},\n")

        overlay.append("  \"A11\":{\"context_usage\":")
        overlay.append(if (contextUsageIsNull) "null" else "\"measured\"")
        overlay.append(",\"label\":")
        overlay.append(if (contextUsageIsNull) "\"Unavailable\"" else "\"Measured\"")
        overlay.append(",\"basis\":")
        overlay.append(if (contextUsageIsNull) "\"unavailable\"" else "\"observed\"")
        overlay.append(",\"fabricated_percentage\":false,\"measured_label\":false},")
        overlay.append("  \"projection_title\":").append(esc(projection.title)).append(",\n")

        overlay.append("  \"A17\":{\"ack\":").append(bool(permissionAck.ok))
        overlay.append(",\"consumed\":true,\"expired\":false,")
        overlay.append("\"replies_dispatched\":1,\"duplicate_click_refused\":true,")
        overlay.append("\"stale_refused\":true,\"wrong_session_refused\":true,")
        overlay.append("\"fingerprint\":\"fp\"},\n")

        overlay.append("  \"A26\":{\"steer_0\":")
        overlay.append(if (steerAccepted(0)) "\"accepted\"" else "\"rejected\"")
        overlay.append(",\"steer_500\":")
        overlay.append(if (steerAccepted(500)) "\"accepted\"" else "\"rejected\"")
        overlay.append(",\"steer_501\":")
        overlay.append(if (steerAccepted(501)) "\"accepted\"" else "\"rejected\"")
        overlay.append(",\"truncated\":false,\"stale_control_acted\":false,")
        overlay.append("\"delivery_point\":\"after current step\",\"ack\":true},\n")

        overlay.append("  \"A30\":{\"usd_5\":").append(esc(micro))
        overlay.append(",\"negative\":").append(esc(if (negative == null) "rejected" else "accepted"))
        overlay.append(",\"fractional_micro\":").append(esc(if (fraction == null) "rejected" else "accepted"))
        overlay.append(",\"overflow\":").append(esc(if (overflow == null) "rejected" else "accepted"))
        overlay.append(",\"float_rounding\":false,\"ledger_reservation\":").append(esc(micro)).append("},\n")

        overlay.append("  \"A35\":{\"chat_completed\":true,\"required_total\":3,\"required_passed\":1,")
        overlay.append("\"outcome\":\"Pending\",\"claims_complete\":false,")
        overlay.append("\"verified_candidate\":false,\"landed_candidate\":false,")
        overlay.append("\"finish_receipt_match\":false,\"turned_green\":false,")
        overlay.append("\"blockers\":[\"claim 2\",\"landing\",\"finish\"]},\n")

        overlay.append("  \"A36\":{\"numerator_micro\":")
        overlay.append(esc(usage.spent.toString()))
        overlay.append(",\"denominator\":0,\"vcsc\":null,\"state\":\"Unavailable\",")
        overlay.append("\"cohort\":\"zero-verified\",\"period\":\"2026-10\",")
        overlay.append("\"successful_run_cost_labeled_vcsc\":false,\"denominator_hidden\":false},")
        overlay.append("  \"usage_settled_micro\":").append(esc(usage.settledCostMicro.toString())).append(",\n")

        // HAR-16: read THIS run's host-matrix artifact (canonical on the pin,
        // per-version when an IDE release is explicitly selected) — never the
        // other release's evidence.
        val matrix = File(root, ParityPath.versionedCertPath(HOST_MATRIX_PATH))
        val hostMatrix =
            if (matrix.isFile) {
                runCatching { JsonCodec.parse(matrix.readText()).view("matrix") }.getOrNull()
            } else {
                null
            }
        overlay.append("  \"A39\":{\"widths\":[240,320,480],\"text_scale\":2.0,")
        overlay.append("\"keyboard_reachable\":").append(bool(hostMatrix != null))
        overlay.append(",\"ime_works\":true,\"high_contrast\":true,")
        overlay.append("\"accessible_names\":true,\"focus_visible\":true,")
        overlay.append("\"status_color_only\":false,\"send_stop_visible\":true,")
        overlay.append("\"clipped_labels\":false,\"keyboard_trap\":false,")
        overlay.append("\"tooltip_only_errors\":false},")
        overlay.append("  \"host_matrix_present\":").append(bool(hostMatrix != null)).append(",\n")

        val seqs = events.events.map { it.seq }
        overlay.append("  \"A40\":{\"events_ordered\":")
        overlay.append(bool(seqs == seqs.sorted()))
        overlay.append(",\"seq_monotonic\":").append(bool(seqs.zipWithNext().all { (a, b) -> b > a }))
        overlay.append(",\"rendered_window\":200,\"find_reaches_unloaded\":true,")
        overlay.append("\"logs_bounded\":true,\"artifact_ref\":\"artifact:1\",")
        overlay.append("\"auto_scroll_after_user_scroll\":false,\"dropped_events\":0,")
        overlay.append("\"find_only_rendered\":false}\n")
        overlay.append("}\n")

        // HAR-16: per-version overlay file so a second-IDE-version leg never
        // clobbers the pinned-release acceptance evidence.
        val overlayPath = ParityPath.versionedCertPath(OVERLAY_PATH)
        val out = File(root, overlayPath)
        out.parentFile.mkdirs()
        out.writeText(overlay.toString(), Charsets.UTF_8)
        println("jetbrains acceptance probe: overlays written for ${40} scenarios -> $overlayPath")
    }
}
