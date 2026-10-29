// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir,separate-process
// Auto-start lifecycle tests against the ACTUAL plugin adapter (audit item
// 2): the factory's own lifecycle seam
// ([FaktorToolWindowFactory.startLifecycleAutoStart]) drives the REAL
// FaktorFrontendService against canned fake-daemon PROCESSES — not the
// Swing standalone demo:
//
//  - a trusted install root (checksum-verified executable inside
//    ~/.faktor/install's shape) STARTS the service and surfaces the ready
//    state on the panel;
//  - a binary OUTSIDE the trusted root is refused TYPED
//    (`outside_trusted_install`) and NOTHING executes (a would-be spawn
//    marker file proves it);
//  - a verified trusted binary that dies before readiness surfaces a typed
//    `startup_failed` — never a fabricated "ready";
//  - a live serve.lock/serve.password holder of the data dir is ADOPTED
//    through the daemon's reuse contract (no second spawn, resolve never
//    runs), and Stop detaches this host only.
//
// Isolation: every case uses its own temp home/root/data dir and its own
// fake-daemon processes; nothing reads or writes the developer's ~/.faktor.
package dev.faktor.frontend

import dev.faktor.backend.DaemonBinary
import java.net.InetSocketAddress
import java.nio.file.Files
import java.nio.file.Path
import java.nio.file.Paths
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Test

class ToolWindowAutoStartTest {

    // ------------------------------------------------------- policy matrix

    @Test
    fun decidePolicyStartsOnlyVerifiedExecutablesInsideTheTrustedRoot() {
        val root = Files.createTempDirectory("faktor-autostart-policy-")
        try {
            val trustedRoot = root.resolve("home").resolve(".faktor").resolve("install")
            Files.createDirectories(trustedRoot)
            val inside = trustedRoot.resolve("launcher")
            Files.write(inside, "fake".toByteArray(Charsets.UTF_8))
            val outside = root.resolve("elsewhere").resolve("cli")
            Files.createDirectories(outside.parent)
            Files.write(outside, "fake".toByteArray(Charsets.UTF_8))
            val verified = { path: Path -> DaemonBinary.Trust.Verified("sha256 record ok") }

            val start = ToolWindowAutoStart.decide(
                { DaemonBinary.Resolution(inside, "signed install layout") },
                trustedRoot,
                verified
            )
            assertTrue(start is AutoStartDecision.Start, "$start")

            val untrusted = ToolWindowAutoStart.decide(
                { DaemonBinary.Resolution(outside, "application setting") },
                trustedRoot,
                verified
            )
            assertEquals(
                ToolWindowAutoStart.KIND_OUTSIDE_TRUSTED_INSTALL,
                (untrusted as AutoStartDecision.Refused).kind
            )

            val unverified = ToolWindowAutoStart.decide(
                { DaemonBinary.Resolution(inside, "signed install layout") },
                trustedRoot
            ) { DaemonBinary.Trust.Unverified("no checksum record") }
            assertEquals(
                ToolWindowAutoStart.KIND_TRUST_UNVERIFIED,
                (unverified as AutoStartDecision.Refused).kind
            )

            val mismatch = ToolWindowAutoStart.decide(
                { DaemonBinary.Resolution(inside, "signed install layout") },
                trustedRoot
            ) { DaemonBinary.Trust.Refused("checksum mismatch") }
            assertEquals(
                ToolWindowAutoStart.KIND_TRUST_REFUSED,
                (mismatch as AutoStartDecision.Refused).kind
            )

            val missing = ToolWindowAutoStart.decide(
                {
                    throw DaemonBinary.MissingDaemonException("Faktor daemon not found")
                },
                trustedRoot,
                verified
            )
            assertEquals(
                ToolWindowAutoStart.KIND_BINARY_MISSING,
                (missing as AutoStartDecision.Refused).kind
            )
        } finally {
            root.toFile().deleteRecursively()
        }
    }

    // ------------------------------------------ trusted root starts (spawn)

    @Test
    fun trustedInstallRootStartsTheServiceThroughTheFactoryAdapter() {
        val root = Files.createTempDirectory("faktor-autostart-trusted-")
        var panel: FaktorChatPanel? = null
        var service: FaktorFrontendService? = null
        try {
            val trustedRoot = root.resolve("home").resolve(".faktor").resolve("install")
            Files.createDirectories(trustedRoot)
            val dataDir = root.resolve("data")
            Files.createDirectories(dataDir)
            val launcher = writeFakeDaemonScript(trustedRoot.resolve("launcher"), dataDirArg = null)
            service = FaktorFrontendService({ launcher }, dataDir)
            panel = FaktorChatPanel(service)
            FaktorToolWindowFactory.startLifecycleAutoStart(
                service,
                panel,
                resolve = { DaemonBinary.Resolution(launcher, "signed install layout") },
                trustedRoot = trustedRoot
            )
            await("the trusted-install service starts") { service!!.isRunning() }
            assertTrue(service!!.lastStartAdopted == false, "a fresh spawn, not an adoption")
            await("the panel surfaces the ready state") {
                panel!!.diagnosticsReadinessForTest().startsWith("ready")
            }
            val text = panel!!.transcriptTextForTest()
            assertTrue(text.contains("daemon ready: version 0.0.0-autostart-test"), text)
        } finally {
            panel?.shutdown()
            service?.stop()
            root.toFile().deleteRecursively()
        }
    }

    // ------------------------------------------- untrusted refuses (typed)

    @Test
    fun untrustedInstallIsRefusedTypedAndNothingExecutes() {
        val root = Files.createTempDirectory("faktor-autostart-untrusted-")
        var panel: FaktorChatPanel? = null
        var service: FaktorFrontendService? = null
        try {
            val trustedRoot = root.resolve("home").resolve(".faktor").resolve("install")
            Files.createDirectories(trustedRoot)
            val dataDir = root.resolve("data")
            Files.createDirectories(dataDir)
            // The would-be daemon OUTSIDE the trusted root: if the policy ever
            // executed it, the marker file would appear.
            val marker = root.resolve("executed.marker")
            val outside = root.resolve("elsewhere").resolve("daemon")
            Files.createDirectories(outside.parent)
            Files.write(
                outside,
                ("#!/bin/sh\ntouch \"$marker\"\nexit 0\n").toByteArray(Charsets.UTF_8)
            )
            outside.toFile().setExecutable(true, false)
            service = FaktorFrontendService({ outside }, dataDir)
            panel = FaktorChatPanel(service)
            FaktorToolWindowFactory.startLifecycleAutoStart(
                service,
                panel,
                resolve = { DaemonBinary.Resolution(outside, "application setting") },
                trustedRoot = trustedRoot
            ) { DaemonBinary.Trust.Verified("injected for the policy matrix") }
            await("the typed refusal surfaces on the panel") {
                panel!!.diagnosticsReadinessForTest().startsWith("auto-start refused")
            }
            val readiness = panel!!.diagnosticsReadinessForTest()
            assertTrue(
                readiness.contains(ToolWindowAutoStart.KIND_OUTSIDE_TRUSTED_INSTALL),
                readiness
            )
            val text = panel!!.transcriptTextForTest()
            assertTrue(text.contains("outside the trusted Faktor install"), text)
            assertTrue(text.contains("Start service"), "the refusal names the recovery: $text")
            assertEquals(false, service!!.isRunning(), "nothing may run for an untrusted binary")
            assertEquals(false, Files.exists(marker), "the untrusted executable must never run")
        } finally {
            panel?.shutdown()
            service?.stop()
            root.toFile().deleteRecursively()
        }
    }

    // ------------------------------------- startup failure surfaces typed

    @Test
    fun trustedBinaryThatDiesBeforeReadinessSurfacesTyped() {
        val root = Files.createTempDirectory("faktor-autostart-failure-")
        var panel: FaktorChatPanel? = null
        var service: FaktorFrontendService? = null
        try {
            val trustedRoot = root.resolve("home").resolve(".faktor").resolve("install")
            Files.createDirectories(trustedRoot)
            val dataDir = root.resolve("data")
            Files.createDirectories(dataDir)
            val launcher = trustedRoot.resolve("launcher")
            Files.write(launcher, "#!/bin/sh\nexit 1\n".toByteArray(Charsets.UTF_8))
            launcher.toFile().setExecutable(true, false)
            Files.write(
                launcher.resolveSibling("launcher.sha256"),
                (DaemonBinary.sha256(launcher) + "  launcher\n").toByteArray(Charsets.UTF_8)
            )
            service = FaktorFrontendService({ launcher }, dataDir)
            panel = FaktorChatPanel(service)
            FaktorToolWindowFactory.startLifecycleAutoStart(
                service,
                panel,
                resolve = { DaemonBinary.Resolution(launcher, "signed install layout") },
                trustedRoot = trustedRoot
            )
            await("the typed startup failure surfaces") {
                panel!!.diagnosticsReadinessForTest().startsWith("auto-start refused")
            }
            val readiness = panel!!.diagnosticsReadinessForTest()
            assertTrue(readiness.contains(ToolWindowAutoStart.KIND_STARTUP_FAILED), readiness)
            assertTrue(
                readiness.contains("exited with code 1"),
                "the typed failure carries the exact cause: $readiness"
            )
            val text = panel!!.transcriptTextForTest()
            assertTrue(text.contains("The service did not start"), text)
            assertEquals(false, service!!.isRunning())
            assertEquals(false, panel!!.diagnosticsReadinessForTest().startsWith("ready"))
        } finally {
            panel?.shutdown()
            service?.stop()
            root.toFile().deleteRecursively()
        }
    }

    // ------------------------------------------ serve.lock adoption (reuse)

    @Test
    fun liveServeLockHolderIsAdoptedAndStopDetachesOnly() {
        val root = Files.createTempDirectory("faktor-autostart-adopt-")
        var panel: FaktorChatPanel? = null
        var service: FaktorFrontendService? = null
        var daemon: Process? = null
        try {
            val dataDir = root.resolve("data")
            Files.createDirectories(dataDir)
            val password = "adopt-password"
            val daemonBuilder = ProcessBuilder(
                javaBinary(), "-cp", System.getProperty("java.class.path"),
                ToolWindowFakeDaemonMain::class.java.name,
                "--data-dir", dataDir.toString()
            ).redirectError(ProcessBuilder.Redirect.INHERIT)
            daemonBuilder.environment()["FAKTOR_SERVER_PASSWORD"] = password
            daemon = daemonBuilder.start()
            await("the standalone daemon records its serve.lock") {
                Files.isRegularFile(dataDir.resolve("serve.lock")) &&
                    Files.isRegularFile(dataDir.resolve("serve.password"))
            }
            val resolveCalls = AtomicInteger(0)
            val serviceResolve: () -> Path = {
                resolveCalls.incrementAndGet()
                throw DaemonBinary.MissingDaemonException(
                    "adoption must happen before any binary resolution"
                )
            }
            val trustedRoot = root.resolve("home").resolve(".faktor").resolve("install")
            Files.createDirectories(trustedRoot)
            val executable = trustedRoot.resolve("launcher")
            Files.write(executable, "fake".toByteArray(Charsets.UTF_8))
            service = FaktorFrontendService(serviceResolve, dataDir)
            panel = FaktorChatPanel(service)
            FaktorToolWindowFactory.startLifecycleAutoStart(
                service,
                panel,
                resolve = { DaemonBinary.Resolution(executable, "signed install layout") },
                trustedRoot = trustedRoot
            ) { DaemonBinary.Trust.Verified("injected for the adoption matrix") }
            await("the live holder is adopted") { service!!.isRunning() }
            assertEquals(true, service!!.lastStartAdopted, "the start must be an adoption")
            assertEquals(0, resolveCalls.get(), "adoption never resolves or spawns a binary")
            await("the panel surfaces the adopted connection") {
                panel!!.transcriptTextForTest()
                    .contains("Connected to the running Faktor service")
            }
            // Stop detaches THIS host only: the spawn owner keeps the daemon.
            service!!.stop()
            assertEquals(false, service!!.isRunning())
            assertTrue(daemon.isAlive, "an adopted daemon is never killed by a detach")
        } finally {
            panel?.shutdown()
            service?.stop()
            daemon?.destroyForcibly()
            root.toFile().deleteRecursively()
        }
    }

    // ------------------------------------------------------------ harness

    /**
     * The fake trusted-install daemon: serves the real /native/health +
     * /native/ready contracts (Bearer = FAKTOR_SERVER_PASSWORD), writes the
     * serve.lock/serve.password reuse record into its data dir exactly like
     * the real daemon, and prints the frozen startup line.
     */
    private fun writeFakeDaemonScript(target: Path, dataDirArg: String?): Path {
        val passThrough = "\"${'$'}@\""
        val content = "#!/bin/sh\n" +
            "exec \"${javaBinary()}\" -cp \"${System.getProperty("java.class.path")}\" " +
            "${ToolWindowFakeDaemonMain::class.java.name} " +
            (if (dataDirArg == null) passThrough else "--data-dir \"$dataDirArg\" $passThrough") + "\n"
        Files.write(target, content.toByteArray(Charsets.UTF_8))
        target.toFile().setExecutable(true, false)
        Files.write(
            target.resolveSibling(target.fileName.toString() + ".sha256"),
            (DaemonBinary.sha256(target) + "  ${target.fileName}\n").toByteArray(Charsets.UTF_8)
        )
        return target
    }

    private fun javaBinary(): String = Paths.get(
        System.getProperty("java.home"), "bin", "java"
    ).toString()

    private fun await(what: String, timeoutMs: Long = 30_000L, condition: () -> Boolean) {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            if (condition()) return
            Thread.sleep(50L)
        }
        assertTrue(condition(), "timed out waiting for: $what")
    }
}

/**
 * The fake daemon PROCESS behind [ToolWindowAutoStartTest]: health + ready
 * with the bearer claim, the serve.lock/serve.password reuse record, and the
 * frozen startup line — the same contract surface the real daemon owns.
 */
object ToolWindowFakeDaemonMain {
    @JvmStatic
    fun main(args: Array<String>) {
        val dataDirIndex = args.indexOf("--data-dir")
        val dataDir = args.getOrNull(dataDirIndex + 1)?.let { Paths.get(it) }
        val server = com.sun.net.httpserver.HttpServer.create(InetSocketAddress("127.0.0.1", 0), 0)
        val password = System.getenv("FAKTOR_SERVER_PASSWORD") ?: ""
        server.createContext("/native/health") { exchange ->
            val auth = exchange.requestHeaders.getFirst("Authorization")
            if (auth == "Bearer $password") {
                val body =
                    "{\"ok\":true,\"version\":\"0.0.0-autostart-test\"}".toByteArray(Charsets.UTF_8)
                exchange.responseHeaders.add("Content-Type", "application/json")
                exchange.sendResponseHeaders(200, body.size.toLong())
                exchange.responseBody.use { it.write(body) }
            } else {
                exchange.sendResponseHeaders(401, -1)
            }
            exchange.close()
        }
        server.createContext("/native/ready") { exchange ->
            val body = "{\"ready\":true}".toByteArray(Charsets.UTF_8)
            exchange.responseHeaders.add("Content-Type", "application/json")
            exchange.sendResponseHeaders(200, body.size.toLong())
            exchange.responseBody.use { it.write(body) }
            exchange.close()
        }
        server.start()
        val port = server.address.port
        val pid = java.lang.management.ManagementFactory.getRuntimeMXBean().name
            .substringBefore('@').toLongOrNull() ?: 0L
        if (dataDir != null) {
            Files.createDirectories(dataDir)
            // The daemon's reuse contract, written like the real serve_lock.
            Files.write(
                dataDir.resolve("serve.lock"),
                "{\"pid\":$pid,\"port\":$port}".toByteArray(Charsets.UTF_8)
            )
            Files.write(
                dataDir.resolve("serve.password"),
                password.toByteArray(Charsets.UTF_8)
            )
        }
        println("faktor server listening on http://127.0.0.1:$port")
        System.out.flush()
        while (true) {
            Thread.sleep(60_000L)
        }
    }
}
