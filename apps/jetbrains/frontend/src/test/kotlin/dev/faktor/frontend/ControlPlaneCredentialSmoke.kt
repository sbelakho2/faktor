// Control-plane credential smoke: PasswordSafe rows (fake), the store
// authority, the Settings-panel write-through, sign-out removal and the
// production construction path (FaktorFrontendService receives the token the
// vault resolves). No display and no daemon are needed: Swing components are
// created but never shown.
package dev.faktor.frontend

import dev.faktor.backend.NativeClient
import dev.faktor.shared.NativeApiException
import java.nio.file.Files
import java.nio.file.Paths

/** Fake PasswordSafe rows keyed by "<service>|<account>" (endpoint|org). */
private class FakePasswordSafe {
    val rows = LinkedHashMap<String, String>()
    val operations = ArrayList<String>()

    fun rowKey(service: String, account: String): String = "$service|$account"

    fun vault(): SecretVault = object : SecretVault {
        override fun getPassword(service: String, account: String): String? {
            operations.add("get:$service|$account")
            return rows[rowKey(service, account)]
        }

        override fun setPassword(service: String, account: String, password: String?) {
            operations.add("set:$service|$account")
            val key = rowKey(service, account)
            if (password == null) {
                rows.remove(key)
            } else {
                rows[key] = password
            }
        }
    }
}

private class FakeScopeStore(var scope: ControlPlaneScope? = null) : ControlPlaneScopeStore {
    override fun read(): ControlPlaneScope? = scope

    override fun write(scope: ControlPlaneScope) {
        this.scope = scope
    }
}

private class FakeSessionStore(var sessionId: String? = null) : ControlPlaneSessionStore {
    override fun read(): String? = sessionId

    override fun write(sessionId: String?) {
        this.sessionId = sessionId
    }
}

object ControlPlaneCredentialSmoke {

    private var failures = 0

    @JvmStatic
    fun main(args: Array<String>) {
        val vaultBacking = FakePasswordSafe()
        val store = ControlPlaneCredentialStore(vaultBacking.vault())
        val scope = ControlPlaneScope("https://CP.Example/", "org-1")

        step("store writes ONE PasswordSafe row keyed by endpoint+organization") {
            store.store(scope, "cp-secret-token")
            assertEquals(
                listOf("Faktor Control Plane|https://cp.example|org-1"),
                vaultBacking.rows.keys.toList(),
                "one credential row, no other rows"
            )
            assertEquals("cp-secret-token", store.resolve(scope))
            // Blank credentials are refused and write nothing.
            try {
                store.store(scope, "   ")
                fail("a blank credential must be refused")
            } catch (e: IllegalArgumentException) {
                assertTrue(e.message?.contains("empty") == true, e.message)
            }
            assertEquals("cp-secret-token", store.resolve(scope), "a refused blank kept the old row")
        }

        step("resolve returns null without a scope or a matching row") {
            assertEquals(null, store.resolve(null))
            assertEquals(null, store.resolve(ControlPlaneScope("", "org-1")))
            assertEquals(null, store.resolve(ControlPlaneScope("https://cp.example", "  ")))
            assertEquals(null, store.resolve(ControlPlaneScope("https://other.example", "org-1")))
        }

        step("endpoint and organization are separate rows; delete is exact") {
            val otherOrg = ControlPlaneScope("https://cp.example", "org-2")
            val otherEndpoint = ControlPlaneScope("https://eu.example", "org-1")
            store.store(otherOrg, "token-org-2")
            store.store(otherEndpoint, "token-eu")
            assertEquals(3, vaultBacking.rows.size)
            assertEquals("token-org-2", store.resolve(otherOrg))
            assertEquals("token-eu", store.resolve(otherEndpoint))
            assertTrue(store.delete(otherOrg), "the exact row must be deleted")
            assertEquals(null, store.resolve(otherOrg))
            assertEquals("cp-secret-token", store.resolve(scope), "other rows survive")
            assertEquals(2, vaultBacking.rows.size)
            assertTrue(!store.delete(otherOrg), "a second delete reports no credential")
        }

        step("Settings panel writes through the credential store and clears the token field") {
            var saved: List<String>? = null
            var logout = 0
            val panel = SettingsPanel()
            panel.setListener(object : SettingsPanel.Listener {
                override fun onRefreshProviders() {}

                override fun onProviderSelectionChanged(provider: String, model: String) {}

                override fun onControlPlaneCredential(
                    endpoint: String,
                    organization: String,
                    sessionId: String,
                    token: String
                ) {
                    saved = listOf(endpoint, organization, sessionId, token)
                }

                override fun onControlPlaneLogout() {
                    logout += 1
                }
            })
            // An incomplete credential is refused loudly, never written; the
            // auth-session id is optional and does not gate the save.
            panel.enterControlPlaneCredential("https://cp.example", "org-1", "", "")
            panel.submitControlPlaneCredential()
            assertEquals(null, saved)
            assertTrue(
                panel.controlPlaneStatusText().contains("required"),
                panel.controlPlaneStatusText()
            )
            panel.enterControlPlaneCredential("https://cp.example", "org-1", "ses-1", "panel-token")
            panel.submitControlPlaneCredential()
            assertEquals(
                listOf("https://cp.example", "org-1", "ses-1", "panel-token"),
                saved
            )
            assertEquals("", panel.controlPlaneTokenText(), "the token field is cleared after save")
            assertEquals("ses-1", panel.controlPlaneSessionText())
            panel.submitControlPlaneLogout()
            assertEquals(1, logout)
        }

        step("panel sign-in stores in the vault, persists scope + session, sign-out removes the row") {
            val backing = FakePasswordSafe()
            val credentialStore = ControlPlaneCredentialStore(backing.vault())
            val scopeStore = FakeScopeStore()
            val sessionStore = FakeSessionStore()
            val service = FaktorFrontendService(
                Paths.get("unused-binary"),
                Paths.get(System.getProperty("java.io.tmpdir"), "faktor-credential-smoke")
            )
            val panel = FaktorChatPanel(service, credentialStore, scopeStore, sessionStore)
            try {
                panel.settingsView().enterControlPlaneCredential(
                    "https://cp.example",
                    "org-9",
                    "ses-9",
                    "panel-secret"
                )
                panel.settingsView().submitControlPlaneCredential()
                assertEquals(
                    "panel-secret",
                    backing.rows["Faktor Control Plane|https://cp.example|org-9"]
                )
                assertEquals(
                    ControlPlaneScope("https://cp.example", "org-9"),
                    scopeStore.scope,
                    "the non-secret coordinates are persisted, the secret is not"
                )
                assertEquals("ses-9", sessionStore.sessionId, "the non-secret session id is persisted")
                assertTrue(
                    !backing.rows.keys.any { it.contains("panel-secret") },
                    "the row key must not contain the secret"
                )
                assertTrue(service.controlPlaneTokenConfigured(), "the running service picks the token up")
                assertEquals("", panel.settingsView().controlPlaneTokenText())
                panel.settingsView().submitControlPlaneLogout()
                assertEquals(0, backing.rows.size, "sign-out removes the PasswordSafe row")
                assertEquals(null, sessionStore.sessionId, "sign-out clears the dead session id")
                assertTrue(!service.controlPlaneTokenConfigured())
            } finally {
                panel.shutdown()
            }
        }

        step("an external PasswordSafe change reaches the client; removal clears the old token") {
            val backing = FakePasswordSafe()
            val credentialStore = ControlPlaneCredentialStore(backing.vault())
            val scopeStore = FakeScopeStore(ControlPlaneScope("https://cp.example", "org-1"))
            val sessionStore = FakeSessionStore("ses-1")
            val scope = scopeStore.scope!!
            credentialStore.store(scope, "initial-token")
            val service = FaktorFrontendService(
                Paths.get("unused-binary"),
                Paths.get(System.getProperty("java.io.tmpdir"), "faktor-credential-watch")
            )
            service.setControlToken(credentialStore.resolve(scope))
            val panel = FaktorChatPanel(service, credentialStore, scopeStore, sessionStore)
            try {
                // The first poll reconciles the client with the vault value.
                assertTrue(panel.pollControlPlaneWatcherOnce(), "the first poll applies")
                assertEquals("initial-token", service.currentControlPlaneToken())
                // An EXTERNAL PasswordSafe write (another IDE / keychain UI)
                // reaches the RUNNING client without a restart.
                backing.rows["Faktor Control Plane|https://cp.example|org-1"] = "rotated-token"
                assertTrue(panel.pollControlPlaneWatcherOnce(), "the rotation is observed")
                assertEquals("rotated-token", service.currentControlPlaneToken())
                // An EXTERNAL removal clears the old token instead of leaving
                // it live until the next restart.
                backing.rows.remove("Faktor Control Plane|https://cp.example|org-1")
                assertTrue(panel.pollControlPlaneWatcherOnce(), "the removal is observed")
                assertEquals(null, service.currentControlPlaneToken(), "the old token is cleared")
                assertTrue(!service.controlPlaneTokenConfigured())
                assertTrue(!panel.pollControlPlaneWatcherOnce(), "an unchanged row does not fire")
            } finally {
                panel.shutdown()
            }
        }

        step("production construction path supplies the vault token to the client") {
            val backing = FakePasswordSafe()
            val credentialStore = ControlPlaneCredentialStore(backing.vault())
            val scopeStore = FakeScopeStore(ControlPlaneScope("https://cp.example", "org-1"))
            credentialStore.store(scopeStore.scope!!, "factory-token")
            // Exactly the FaktorToolWindowFactory shape.
            val service = FaktorFrontendService(
                Paths.get("unused-binary"),
                Paths.get(System.getProperty("java.io.tmpdir"), "faktor-credential-factory"),
                controlToken = credentialStore.resolve(scopeStore.read())
            )
            assertTrue(service.controlPlaneTokenConfigured(), "the factory supplies the vault token")
            val client = NativeClient("http://127.0.0.1:9", "daemon-password")
            assertEquals(
                "factory-token",
                credentialStore.resolve(scopeStore.read()),
                "the credential handed to the client is the vault value"
            )
            assertTrue(client.bearerToken == "daemon-password")
            service.setControlToken(null)
            assertTrue(!service.controlPlaneTokenConfigured(), "sign-out clears the service token")
        }

        step("sign-out compare-and-delete: a rotation during the in-flight revoke keeps the NEW credential") {
            val backing = FakePasswordSafe()
            val credentialStore = ControlPlaneCredentialStore(backing.vault())
            val scope = ControlPlaneScope("https://cp.example", "org-1")
            credentialStore.store(scope, "old-token")
            val scopeStore = FakeScopeStore(scope)
            val sessionStore = FakeSessionStore("ses-1")
            val presented = ArrayList<String>()
            val server = com.sun.net.httpserver.HttpServer.create(
                java.net.InetSocketAddress("127.0.0.1", 0), 0
            )
            server.createContext("/native/health") { exchange ->
                respondJson(exchange, 200, "{\"ok\":true,\"version\":\"9.9.9\"}")
            }
            server.createContext("/native/ready") { exchange ->
                respondJson(exchange, 200, "{\"ready\":true}")
            }
            server.createContext("/native/sso/logout") { exchange ->
                presented.add(exchange.requestHeaders.getFirst("x-faktor-control-token") ?: "")
                // The external rotation (another IDE / keychain UI) lands
                // WHILE the revoke is in flight.
                backing.rows["Faktor Control Plane|https://cp.example|org-1"] = "rotated-token"
                respondJson(exchange, 200, "{\"ok\":true,\"revoked\":true,\"alreadyRevoked\":false}")
            }
            server.start()
            val java = Paths.get(
                System.getProperty("java.home"), "bin",
                if (System.getProperty("os.name", "").lowercase().contains("win")) "java.exe" else "java"
            ).toString()
            val process = ProcessBuilder(java, "-version").start()
            println("  fake control-plane daemon on ${server.address.port}")
            val service = FaktorFrontendService(
                Paths.get("unused"),
                Paths.get(System.getProperty("java.io.tmpdir"), "faktor-credential-rotate")
            )
            try {
                val connection = dev.faktor.backend.BackendConnection(
                    server.address.port, "smoke-password", process,
                    dev.faktor.backend.StdoutSink(process)
                )
                service.attachConnection(connection, stopAction = { process.destroyForcibly() })
                service.setControlToken("old-token")
                val panel = FaktorChatPanel(service, credentialStore, scopeStore, sessionStore)
                panel.settingsView().submitControlPlaneLogout()
                assertEquals(
                    listOf("old-token"), presented,
                    "the CAPTURED token must be presented, never the rotated one"
                )
                assertEquals(
                    "rotated-token",
                    backing.rows["Faktor Control Plane|https://cp.example|org-1"],
                    "the NEW credential must survive the sign-out"
                )
                assertEquals(
                    "rotated-token", service.currentControlPlaneToken(),
                    "the NEW credential must stay live"
                )
                assertEquals("ses-1", sessionStore.sessionId, "the new session id is not cleared")
                assertTrue(
                    panel.settingsView().controlPlaneStatusText().contains("rotated"),
                    panel.settingsView().controlPlaneStatusText()
                )
                panel.shutdown()
            } finally {
                service.stop()
                server.stop(0)
                process.destroyForcibly()
            }
        }

        if (args.isNotEmpty()) {
            step("panel sign-out hits /native/sso/logout on the real daemon and clears the credential") {
                val backing = FakePasswordSafe()
                val credentialStore = ControlPlaneCredentialStore(backing.vault())
                val scopeStore = FakeScopeStore(ControlPlaneScope("https://cp.example", "org-smoke"))
                val sessionStore = FakeSessionStore("ses-smoke")
                val scope = scopeStore.scope!!
                credentialStore.store(scope, "real-daemon-cp-token")
                val dataDir = Files.createTempDirectory("faktor-credential-daemon-")
                val service = FaktorFrontendService(
                    Paths.get(args[0]),
                    dataDir,
                    controlToken = credentialStore.resolve(scope)
                )
                val panel = FaktorChatPanel(service, credentialStore, scopeStore, sessionStore)
                try {
                    service.start()
                    assertTrue(service.isRunning(), "the real daemon must be adopted")
                    // The live client reaches the REAL route: the daemon
                    // parses the strict body and answers the typed refusal
                    // (no [cloud] section => 409 cloud_disabled); a malformed
                    // body would be a 400 and an unreachable route a
                    // transport error, so both fail this assertion.
                    try {
                        service.revokeControlPlaneSession(scope.organization, "ses-smoke")
                        fail("a cloud-disabled daemon must refuse the revoke")
                    } catch (e: NativeApiException) {
                        assertEquals(
                            409,
                            e.status,
                            "strict body parsed, typed refusal expected: ${e.message}"
                        )
                        assertEquals("cloud_disabled", e.code)
                    }
                    // The panel gesture still removes the local credential
                    // and clears the live client token.
                    panel.settingsView().submitControlPlaneLogout()
                    assertEquals(0, backing.rows.size, "sign-out removes the PasswordSafe row")
                    assertEquals(null, sessionStore.sessionId)
                    assertTrue(!service.controlPlaneTokenConfigured())
                    assertTrue(
                        panel.settingsView().controlPlaneStatusText().contains("removed"),
                        panel.settingsView().controlPlaneStatusText()
                    )
                } finally {
                    panel.shutdown()
                    service.stop()
                }
            }
        }

        println(
            if (failures == 0) {
                "CONTROL PLANE CREDENTIAL SMOKE PASS"
            } else {
                "CONTROL PLANE CREDENTIAL SMOKE FAIL ($failures)"
            }
        )
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

private fun respondJson(exchange: com.sun.net.httpserver.HttpExchange, status: Int, body: String) {
    val bytes = body.toByteArray(Charsets.UTF_8)
    exchange.responseHeaders.add("Content-Type", "application/json")
    exchange.sendResponseHeaders(status, bytes.size.toLong())
    exchange.responseBody.use { it.write(bytes) }
    exchange.close()
}
