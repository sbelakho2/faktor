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

import dev.faktor.shared.BearerAuth
import dev.faktor.shared.NativeRequests
import dev.faktor.shared.ReleaseDigest
import dev.faktor.shared.ReleasePidHandshake
import dev.faktor.shared.ReleasePidLine
import dev.faktor.shared.StartupLine
import dev.faktor.shared.parseNativeHealth
import dev.faktor.shared.parseNativePromptReceipt
import dev.faktor.shared.parseNativeSessionCreated
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths
import java.util.concurrent.TimeUnit
object BackendProcessManagerTest {

    @JvmStatic
    fun runAll() {
        assertFixtureStartupLine()
        assertReleasePidLine()
        assertReleaseDigest()
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
        launcherLifecycleSmoke()
        println(if (failures == 0) "SMOKE PASS" else "SMOKE FAIL ($failures)")
        kotlin.system.exitProcess(if (failures == 0) 0 else 1)
    }

    /**
     * The launcher-lifecycle rows, against fake launcher/release processes:
     *  - launcher-exits-early-still-alive: the release pid is tracked (never
     *    the launcher's) and the launcher's exit is not daemon death;
     *  - launcher-exits-early-daemon-dies-detected: the RELEASE death is
     *    reported once the launcher is gone;
     *  - stop-terminates-release-not-launcher: stop() kills the release even
     *    when a legacy launcher stays resident.
     */
    private fun launcherLifecycleSmoke() {
        val root = Files.createTempDirectory("faktor-launcher-smoke-")
        val pidfile = root.resolve("release.pid")
        try {
            step("launcher exits early: release pid tracked, launcher exit is not death") {
                Files.deleteIfExists(pidfile)
                val manager = BackendProcessManager(
                    fakeLauncherScript(root, "early", announce = false), root,
                    releasePidResolver = { readPidFile(pidfile) }
                )
                val connection = manager.start()
                try {
                    val releasePid = connection.pid()
                    val launcherPid = connection.process.pid()
                    assertTrue(
                        releasePid != launcherPid,
                        "release pid $releasePid must differ from launcher pid $launcherPid"
                    )
                    assertEquals(readPidFile(pidfile), releasePid, "resolved release pid")
                    assertTrue(
                        connection.process.waitFor(5, TimeUnit.SECONDS),
                        "the immediate-exit launcher must exit"
                    )
                    assertTrue(connection.isAlive(), "launcher exit must not read as daemon death")
                    assertTrue(pidAlive(releasePid), "the release must still be running")
                    manager.stop(connection)
                    assertTrue(
                        waitUntil(5_000) { !pidAlive(releasePid) },
                        "stop must terminate the release"
                    )
                    assertTrue(!connection.isAlive(), "a stopped release must report dead")
                    println("  releasePid=$releasePid launcherPid=$launcherPid")
                } finally {
                    manager.stop(connection)
                }
            }

            step("launcher handshake pid line names the RELEASE without a probe") {
                Files.deleteIfExists(pidfile)
                val manager = BackendProcessManager(
                    fakeLauncherScript(root, "early", announce = true), root,
                    releasePidResolver = { null }
                )
                val connection = manager.start()
                try {
                    val releasePid = readPidFile(pidfile)
                    assertTrue(releasePid != null && releasePid > 0, "release pidfile")
                    assertEquals(releasePid, connection.pid(), "the announced pid must be tracked")
                    assertEquals(
                        null, connection.releaseTrustError,
                        "a matching health digest must be trusted"
                    )
                    assertTrue(
                        connection.pid() != connection.process.pid(),
                        "the announced release pid must not be the launcher pid"
                    )
                    assertTrue(connection.process.waitFor(5, TimeUnit.SECONDS), "launcher exit")
                    assertTrue(connection.isAlive(), "release alive after the announced-pid launcher exit")
                } finally {
                    manager.stop(connection)
                }
            }

            step("launcher exits early: RELEASE death is detected once the launcher is gone") {
                Files.deleteIfExists(pidfile)
                val manager = BackendProcessManager(
                    fakeLauncherScript(root, "early", announce = false), root,
                    releasePidResolver = { readPidFile(pidfile) }
                )
                val connection = manager.start()
                try {
                    val releasePid = connection.pid()
                    assertTrue(connection.process.waitFor(5, TimeUnit.SECONDS), "launcher exit")
                    terminatePid(releasePid, force = true)
                    assertTrue(waitUntil(5_000) { !pidAlive(releasePid) }, "release death")
                    assertTrue(!connection.isAlive(), "the RELEASE death must be detected")
                } finally {
                    manager.stop(connection)
                }
            }

            step("stop terminates the RELEASE, not just a resident launcher") {
                Files.deleteIfExists(pidfile)
                val manager = BackendProcessManager(
                    fakeLauncherScript(root, "resident", announce = true), root,
                    releasePidResolver = { readPidFile(pidfile) }
                )
                val connection = manager.start()
                try {
                    val releasePid = connection.pid()
                    assertTrue(
                        releasePid != connection.process.pid(),
                        "release and resident launcher must be distinct processes"
                    )
                    assertTrue(connection.process.isAlive, "the legacy launcher must stay resident")
                    manager.stop(connection)
                    assertTrue(
                        waitUntil(5_000) { !pidAlive(releasePid) },
                        "stop must terminate the RELEASE"
                    )
                    assertTrue(
                        waitUntil(5_000) { !connection.process.isAlive },
                        "the resident launcher must be cleaned up"
                    )
                    assertTrue(!connection.isAlive())
                } finally {
                    manager.stop(connection)
                }
            }

            step("mismatched handshake digest refuses the announced pid, never signals it") {
                Files.deleteIfExists(pidfile)
                val victim = startVictim()
                try {
                    val manager = BackendProcessManager(
                        fakeLauncherScript(
                            root, "early", announce = true,
                            announcePid = victim.pid(),
                            announceDigest = "b".repeat(64)
                        ),
                        root,
                        releasePidResolver = { null }
                    )
                    val connection = manager.start()
                    try {
                        assertEquals(
                            "release_digest_mismatch", connection.releaseTrustError?.code,
                            "a mismatched handshake digest must be refused typed"
                        )
                        assertTrue(
                            connection.pid() != victim.pid(),
                            "the refused announced pid must never be adopted"
                        )
                        assertTrue(connection.process.waitFor(5, TimeUnit.SECONDS), "launcher exit")
                        manager.stop(connection)
                        assertTrue(pidAlive(victim.pid()), "the refused announced pid must never be signalled")
                        assertEquals(
                            "release_digest_mismatch", connection.stopRefusal?.code,
                            "stop must report the typed refusal instead of a silent no-op"
                        )
                    } finally {
                        manager.stop(connection)
                    }
                } finally {
                    terminatePid(victim.pid(), force = true)
                }
            }

            step("absent health digest refuses the announcement, never signals the release") {
                Files.deleteIfExists(pidfile)
                val manager = BackendProcessManager(
                    fakeLauncherScript(root, "early", announce = true, noDigest = true), root,
                    releasePidResolver = { null }
                )
                val connection = manager.start()
                try {
                    val releasePid = readPidFile(pidfile)
                        ?: fail("release pidfile must exist")
                    assertEquals(
                        "release_digest_absent", connection.releaseTrustError?.code,
                        "an absent health digest must refuse the announcement"
                    )
                    assertTrue(connection.pid() != releasePid, "the unverifiable announced pid is not adopted")
                    assertTrue(connection.process.waitFor(5, TimeUnit.SECONDS), "launcher exit")
                    manager.stop(connection)
                    assertTrue(pidAlive(releasePid), "the unverified release must not be signalled")
                    assertEquals("release_digest_absent", connection.stopRefusal?.code)
                    terminatePid(releasePid, force = true)
                } finally {
                    manager.stop(connection)
                }
            }

            step("probe disagreement refuses the announced pid; the probed release wins") {
                Files.deleteIfExists(pidfile)
                val victim = startVictim()
                try {
                    val manager = BackendProcessManager(
                        fakeLauncherScript(
                            root, "early", announce = true,
                            announcePid = victim.pid()
                        ),
                        root,
                        releasePidResolver = { readPidFile(pidfile) }
                    )
                    val connection = manager.start()
                    try {
                        val releasePid = readPidFile(pidfile)
                            ?: fail("release pidfile must exist")
                        assertEquals(
                            "release_pid_probe_mismatch", connection.releaseTrustError?.code,
                            "a probe disagreement must refuse the announcement"
                        )
                        assertEquals(releasePid, connection.pid(), "the probed release pid is adopted")
                        manager.stop(connection)
                        assertTrue(
                            waitUntil(5_000) { !pidAlive(releasePid) },
                            "the probed release must be stopped"
                        )
                        assertTrue(pidAlive(victim.pid()), "the announced victim pid must never be signalled")
                    } finally {
                        manager.stop(connection)
                    }
                } finally {
                    terminatePid(victim.pid(), force = true)
                }
            }

            step("liveness falls back to health when no release pid is resolved") {
                Files.deleteIfExists(pidfile)
                val manager = BackendProcessManager(
                    fakeLauncherScript(root, "early", announce = false), root,
                    releasePidResolver = { null }
                )
                val connection = manager.start()
                try {
                    val releasePid = readPidFile(pidfile)
                        ?: fail("release pidfile must exist")
                    assertTrue(connection.process.waitFor(5, TimeUnit.SECONDS), "launcher exit")
                    assertTrue(
                        connection.isAlive(),
                        "a live daemon with no resolved release pid must report alive via health"
                    )
                    terminatePid(releasePid, force = true)
                    assertTrue(waitUntil(5_000) { !pidAlive(releasePid) }, "release death")
                    assertTrue(!connection.isAlive(), "an unhealthy daemon must report dead")
                } finally {
                    manager.stop(connection)
                }
            }

            step("recycled resolved pid: a dead daemon refuses to signal the cached pid") {
                Files.deleteIfExists(pidfile)
                val victim = startVictim()
                try {
                    val manager = BackendProcessManager(
                        fakeLauncherScript(root, "resident", announce = false), root,
                        // Simulates a stale resolution that now names an
                        // unrelated process: alive, but owning no daemon.
                        releasePidResolver = { victim.pid() }
                    )
                    val connection = manager.start()
                    try {
                        val releasePid = readPidFile(pidfile)
                            ?: fail("release pidfile must exist")
                        assertEquals(victim.pid(), connection.pid(), "the fixture resolves the stale pid")
                        terminatePid(releasePid, force = true)
                        assertTrue(waitUntil(5_000) { !pidAlive(releasePid) }, "release death")
                        assertTrue(waitUntil(5_000) { !connection.isAlive() }, "health to stop answering")
                        manager.stop(connection)
                        assertTrue(
                            pidAlive(victim.pid()),
                            "a pid without health identity must never be signalled"
                        )
                        assertEquals(
                            "stop_identity_unverified", connection.stopRefusal?.code,
                            "the refusal must be typed"
                        )
                    } finally {
                        manager.stop(connection)
                    }
                } finally {
                    terminatePid(victim.pid(), force = true)
                }
            }
        } finally {
            root.toFile().deleteRecursively()
        }
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

// ------------------------------------------------- fake launcher + release

/**
 * A fake bootstrap LAUNCHER: spawns the fake release, forwards its stdout,
 * prints the frozen release-pid handshake line when asked (BEFORE forwarding
 * the startup line), then either exits the moment readiness is seen
 * ("early": the release keeps running, exactly like the new launcher) or
 * stays resident forwarding the exit status ("resident": the legacy
 * launcher). Args: <mode> <announce|quiet> <pidfile> + ignored serve args.
 * Env knobs: `FAKE_LAUNCHER_ANNOUNCE_PID` / `FAKE_LAUNCHER_ANNOUNCE_DIGEST`
 * override the announced identity so trust-binding tests can point the
 * handshake at a victim pid or a mismatched digest.
 */
object FakeLauncherMain {
    @JvmStatic
    fun main(args: Array<String>) {
        val mode = args.getOrNull(0) ?: "early"
        val announce = args.getOrNull(1) == "announce"
        val pidfile = args.getOrNull(2)
        val announcedPid = args.getOrNull(3)?.takeIf { it != "-" }?.toLongOrNull()
        val announcedDigest = args.getOrNull(4) ?: "a".repeat(64)
        val noDigest = args.getOrNull(5) == "nodigest"
        val command = mutableListOf(
            javaBinary(), "-cp", System.getProperty("java.class.path"),
            "dev.faktor.backend.FakeReleaseMain"
        )
        if (pidfile != null) command.add(pidfile)
        command.add(if (noDigest) "nodigest" else "-")
        val child = ProcessBuilder(command)
            .redirectError(ProcessBuilder.Redirect.INHERIT)
            .start()
        val reader = child.inputStream.bufferedReader()
        var startupForwarded = false
        while (true) {
            val line = reader.readLine() ?: break
            if (!startupForwarded && StartupLine.parse(line) != null) {
                startupForwarded = true
                if (announce) {
                    println("faktor release started pid=${announcedPid ?: child.pid()} digest=$announcedDigest")
                }
                println(line)
                System.out.flush()
                if (mode != "resident") {
                    return
                }
                continue
            }
            println(line)
            System.out.flush()
        }
        if (mode == "resident") {
            val code = child.waitFor()
            kotlin.system.exitProcess(if (code == 0) 0 else code)
        }
    }
}

/**
 * A fake RELEASE daemon: records its pid, serves the real `/native/health`
 * contract with the bearer claim (so liveness/trust probes exercise the same
 * shape the daemon serves), prints the frozen startup line, and stays alive
 * until signalled (detached from the launcher). Env
 * `FAKE_RELEASE_NO_DIGEST=1` makes the health version carry no release
 * digest.
 */
object FakeReleaseMain {
    @JvmStatic
    fun main(args: Array<String>) {
        val pidfile = args.getOrNull(0)
        val noDigest = args.getOrNull(1) == "nodigest"
        val digest = "a".repeat(64)
        val version = if (noDigest) "9.9.9" else "9.9.9+release.smoke.$digest"
        val server = com.sun.net.httpserver.HttpServer.create(
            java.net.InetSocketAddress("127.0.0.1", 0), 0
        )
        server.createContext("/native/health") { exchange ->
            val auth = exchange.requestHeaders.getFirst("Authorization")
            if (auth == "Bearer " + System.getenv("FAKTOR_SERVER_PASSWORD")) {
                val body = "{\"ok\":true,\"version\":\"$version\"}".toByteArray(Charsets.UTF_8)
                exchange.responseHeaders.add("Content-Type", "application/json")
                exchange.sendResponseHeaders(200, body.size.toLong())
                exchange.responseBody.use { it.write(body) }
            } else {
                exchange.sendResponseHeaders(401, -1)
            }
            exchange.close()
        }
        server.start()
        val port = server.address.port
        if (pidfile != null) {
            Files.write(Paths.get(pidfile), currentPid().toString().toByteArray(Charsets.UTF_8))
        }
        println("faktor server listening on http://127.0.0.1:$port")
        System.out.flush()
        while (true) {
            Thread.sleep(60_000L)
        }
    }

    private fun currentPid(): Long =
        java.lang.management.ManagementFactory.getRuntimeMXBean().name.substringBefore('@').toLongOrNull() ?: 0L
}

/** Writes an executable launcher wrapper that runs [FakeLauncherMain]. */
private fun fakeLauncherScript(
    root: Path,
    mode: String,
    announce: Boolean,
    announcePid: Long? = null,
    announceDigest: String = "a".repeat(64),
    noDigest: Boolean = false
): Path {
    val pidfile = root.resolve("release.pid")
    val script = root.resolve("launcher-$mode-${if (announce) "announce" else "quiet"}.sh")
    val content = "#!/bin/sh\n" +
        "exec \"${javaBinary()}\" -cp \"${System.getProperty("java.class.path")}\" " +
        "dev.faktor.backend.FakeLauncherMain $mode ${if (announce) "announce" else "quiet"} " +
        "\"$pidfile\" \"${announcePid ?: "-"}\" \"$announceDigest\" " +
        "${if (noDigest) "nodigest" else "-"} \"$@\"\n"
    Files.write(script, content.toByteArray(Charsets.UTF_8))
    script.toFile().setExecutable(true, false)
    return script
}

/** A long-lived unrelated process (the recycled/announced-pid victim). */
private fun startVictim(): Process =
    ProcessBuilder(
        javaBinary(), "-cp", System.getProperty("java.class.path"),
        "dev.faktor.backend.FakeReleaseMain"
    ).redirectError(ProcessBuilder.Redirect.INHERIT).start()

private fun javaBinary(): String = Paths.get(
    System.getProperty("java.home"), "bin",
    if (System.getProperty("os.name", "").lowercase().contains("win")) "java.exe" else "java"
).toString()

private fun readPidFile(pidfile: Path): Long? {
    return try {
        String(Files.readAllBytes(pidfile), Charsets.UTF_8).trim().toLongOrNull()
    } catch (e: Exception) {
        null
    }
}

private fun waitUntil(timeoutMs: Long, predicate: () -> Boolean): Boolean {
    val deadline = System.currentTimeMillis() + timeoutMs
    while (System.currentTimeMillis() < deadline) {
        if (predicate()) return true
        Thread.sleep(25L)
    }
    return predicate()
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

private fun assertReleasePidLine() {
    val digest = "a".repeat(64)
    assertEquals(
        ReleasePidHandshake(4242L, digest),
        ReleasePidLine.parse("faktor release started pid=4242 digest=$digest"),
        "release pid handshake line carries pid AND digest"
    )
    for (hostile in listOf(
        "",
        "faktor release started pid=0 digest=$digest",
        "faktor release started pid=4242 digest=${"A".repeat(64)}",
        "faktor release started pid=4242 digest=${"a".repeat(63)}",
        "faktor release started pid=4242",
        "leading faktor release started pid=4242 digest=$digest",
        "faktor release started pid=4242 digest=$digest trailing"
    )) {
        assertEquals(null, ReleasePidLine.parse(hostile), "hostile handshake must not parse: $hostile")
    }
}

private fun assertReleaseDigest() {
    val digest = "a".repeat(64)
    assertEquals(digest, ReleaseDigest.of("0.9.1+release.0.9.1-abcdef.${digest}"), "health digest")
    assertEquals(null, ReleaseDigest.of("0.9.1"), "no release digest is an honest null")
    assertEquals(null, ReleaseDigest.of("0.9.1+release.0.9.1-abcdef"), "partial suffix is not a digest")
    assertEquals(null, ReleaseDigest.of("0.9.1+release.x.${"A".repeat(64)}"), "uppercase is not a digest")
}

private fun assertFixtureAuthHeader() {
    val password = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    val auth = BearerAuth(password)
    assertEquals(
        "Bearer 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        auth.headerValue,
        "header must be the Faktor-native Bearer claim (no Basic form exists)"
    )
    assertEquals("Authorization", BearerAuth.HEADER_NAME)
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
