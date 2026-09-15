// Dependency-free tests for the split-mode backend manager, plus a
// plain-main smoke runner that exercises the FULL flow against a real
// daemon binary.
//
// No kotlin.test / JUnit: the assertion helpers below use plain Kotlin
// `check`/`require`, so the whole file compiles with plain kotlinc (no
// network, no Gradle) and `compile-and-smoke.sh` runs `BackendSmoke
// <binary>` as a self-contained main that exits 0/1. When the frozen
// frontend lands and the plugin gains a real Gradle build, the same checks
// can be re-annotated with kotlin.test.
package dev.faktor.backend

import dev.faktor.shared.BasicAuth
import dev.faktor.shared.NativeRequests
import dev.faktor.shared.StartupLine
import dev.faktor.shared.parseNativeHealth
import dev.faktor.shared.parseNativePromptReceipt
import dev.faktor.shared.parseNativeSessionCreated
import java.nio.file.Files
import java.nio.file.Paths
object BackendProcessManagerTest {

    @JvmStatic
    fun runAll() {
        assertFixtureStartupLine()
        assertFixtureAuthHeader()
        assertNativeRequestShapes()
        assertNativeResponseParsers()
        assertMissingBinaryFailsLoudly()
        println("PASS all unit assertions")
    }
}

fun assertEquals(expected: Any?, actual: Any?, message: String? = null) {
    check(expected == actual) { "${message ?: "assertion"}: expected $expected, actual $actual" }
}

fun assertTrue(cond: Boolean, message: String? = null) {
    check(cond) { message ?: "assertion failed" }
}

fun fail(message: String): Nothing = throw IllegalStateException(message)

/**
 * The self-contained smoke: runs every step against a REAL daemon binary
 * (args[0]) and exits 0/1. Prints PASS/FAIL per step.
 */
object BackendSmoke {

    private var failures = 0

    @JvmStatic
    fun main(args: Array<String>) {
        if (args.isEmpty()) {
            println("FAIL usage: BackendSmoke <faktor-cli binary path>")
            kotlin.system.exitProcess(1)
        }
        val binary = Paths.get(args[0])
        val dataDir = Files.createTempDirectory("faktor-smoke-")

        step("unit assertions") { BackendProcessManagerTest.runAll() }

        val manager = BackendProcessManager(binary, dataDir)
        var connection: BackendConnection? = null
        var sessionId: String? = null
        try {
            step("start daemon (startup line + port)") {
                connection = manager.start()
                println("  port=${connection!!.port} pid=${connection!!.pid()}")
            }
            val conn = connection
            if (conn != null) {
                val client = NativeClient.forConnection(conn)
                step("native health (GET /native/health)") {
                    val h = client.health()
                    if (!h.ok) fail("health ok=false")
                    if (h.version.isEmpty()) fail("health version is empty")
                }
                step("native readiness (GET /native/ready)") {
                    if (!client.awaitReady(10_000L).ready) fail("daemon never reported ready")
                }
                var sessionId: String? = null
                step("create session (POST /native/session)") {
                    sessionId = client.createSession(
                        "default", "default", null, "kotlin split-mode smoke"
                    ).id
                    if (sessionId!!.isEmpty()) fail("empty session id")
                }
                val sid = sessionId
                if (sid != null) {
                    step("prompt (POST /native/session/{id}/prompt)") {
                        val receipt = client.prompt(sid, "ping from kotlin smoke")
                        if (!receipt.accepted) fail("prompt was not accepted")
                    }
                    step("session state settles (ready_for_next_turn | failed_*)") {
                        val deadline = System.currentTimeMillis() + 20_000L
                        var machine = "unknown"
                        while (System.currentTimeMillis() < deadline) {
                            machine = client.projection(sid).machine
                            if (machine == "ready_for_next_turn" ||
                                machine == "failed_recoverable" ||
                                machine == "failed_permanent"
                            ) {
                                break
                            }
                            Thread.sleep(200L)
                        }
                        println("  state=$machine")
                        if (machine != "ready_for_next_turn" &&
                            machine != "failed_recoverable" &&
                            machine != "failed_permanent"
                        ) {
                            fail("unexpected settled state $machine")
                        }
                    }
                    step("list messages (GET /native/messages)") {
                        val page = client.messages(sid, null, 5)
                        println("  messages=${page.messages.size}")
                        if (page.messages.isEmpty()) fail("expected >= 1 message")
                    }
                }
            }
        } finally {
            if (connection != null) {
                val c = connection!!
                try {
                    manager.stop(c)
                    if (c.process.isAlive) fail("daemon still alive after stop")
                    println("PASS stop daemon")
                } catch (e: Throwable) {
                    failures++
                    println("FAIL stop daemon: ${e.message}")
                    c.process.destroyForcibly()
                }
            }
            dataDir.toFile().deleteRecursively()
        }
        println(if (failures == 0) "SMOKE PASS" else "SMOKE FAIL ($failures)")
        kotlin.system.exitProcess(if (failures == 0) 0 else 1)
    }

    private fun step(name: String, body: () -> Unit) {
        try {
            body()
            println("PASS $name")
        } catch (e: Throwable) {
            failures++
            println("FAIL $name: ${e.message}")
        }
    }
}

// ------------------------------------------------------------------ fixtures

private fun assertFixtureStartupLine() {
    val line = "faktor server listening on http://127.0.0.1:45678"
    val parsed = StartupLine.parse(line)
    assertTrue(parsed != null, "fixture startup line must parse")
    assertEquals(45678, parsed!!.port, "fixture port")
    assertEquals(
        "faktor server listening on http://127.0.0.1:45678",
        parsed.toString(),
        "roundtrip"
    )
    assertEquals(null, StartupLine.parse(""), "empty line must not parse")
    assertEquals(null, StartupLine.parse("faktor server listening on http://127.0.0.1"), "no port")
    assertEquals(
        null,
        StartupLine.parse("faktor server listening on http://127.0.0.1:0x10"),
        "hex port must not parse"
    )
    assertEquals(
        null,
        StartupLine.parse("noise faktor server listening on http://127.0.0.1:45678"),
        "leading junk must not parse"
    )
}

private fun assertFixtureAuthHeader() {
    val password = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    val auth = BasicAuth(password)
    assertEquals(
        "Basic a2lsbzowMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2Nzg5YWJjZGVm",
        auth.headerValue,
        "header must equal the frozen fixture (Basic <base64('kilo:'+password)>)"
    )
    assertEquals("Authorization", BasicAuth.HEADER_NAME)
}

private fun assertNativeRequestShapes() {
    assertEquals(
        "{\"provider\":\"p\",\"model\":\"m\"}",
        NativeRequests.createSession("p", "m"),
        "minimal native create-session body"
    )
    assertEquals(
        "{\"provider\":\"p\",\"model\":\"m\",\"workspace\":\"/ws\",\"title\":\"T\"}",
        NativeRequests.createSession("p", "m", "/ws", "T"),
        "full native create-session body"
    )
    assertEquals(
        "{\"session_id\":\"7\",\"prompt\":\"hi\",\"files\":[\"a.txt\"]}",
        NativeRequests.prompt("7", "hi", listOf("a.txt")),
        "native prompt body"
    )
}

private fun assertNativeResponseParsers() {
    val created = parseNativeSessionCreated(
        "{\"id\":\"7\",\"title\":\"T\",\"created_ms\":1750000000000}"
    )
    assertEquals("7", created.id)
    assertEquals("T", created.title)
    assertEquals(1750000000000L, created.createdMs)

    val receipt = parseNativePromptReceipt("{\"op_id\":\"42\",\"accepted\":true,\"queued\":false}")
    assertEquals("42", receipt.opId)
    assertTrue(receipt.accepted, "accepted")
    assertTrue(!receipt.queued, "not queued")

    val health = parseNativeHealth("{\"ok\":true,\"version\":\"0.5.0\"}")
    assertTrue(health.ok, "health ok")
    assertEquals("0.5.0", health.version)
}

private fun assertMissingBinaryFailsLoudly() {
    try {
        BackendProcessManager(
            Paths.get("/nonexistent/faktor-cli"),
            Files.createTempDirectory("faktor-missing-")
        ).start()
        fail("start() must fail loudly for a missing binary")
    } catch (e: BackendException) {
        assertTrue(e.message!!.contains("not found"), "message: ${e.message}")
    }
}
