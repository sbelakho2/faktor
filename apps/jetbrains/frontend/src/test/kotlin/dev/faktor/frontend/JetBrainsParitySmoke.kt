// JetBrains parity smoke (no display, no JUnit): canned native frames drive
// EVERY parity surface through the real panel code, a raw-socket fake
// daemon drives the real FaktorFrontendService + FaktorChatPanel end to
// end, and the real daemon is used where only it can answer (restart /
// terminal spawn). Families:
//
//   1. upstream pin: every per-file SHA-256 of the vendored JetBrains 7.1.2
//      tree in ui/upstream.json (offline, no network)
//   2. task mode (criteria, mutation mode, completion contract, attachments)
//   3. agent tree (blockers, presentation, pixel identity)
//   4. permissions (pending list, allow/deny replies, unavailable state)
//   5. terminal (session-owned rows, spawn body, lifetime events, output)
//   6. review/tournament (candidate review verdicts, decide gating)
//   7. evidence navigation (refs, retrieval selector, transcript jumps)
//   8. settings + provider selection (registry view, model join, defaults)
//   9. history (durable session list, open)
//  10. restart/reconnect (SSE cursor preserved across reconnect; real
//      daemon restart keeps the durable session)
package dev.faktor.frontend

import dev.faktor.backend.BackendConnection
import dev.faktor.backend.StdoutSink
import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativeCompletionContract
import dev.faktor.shared.NativeMessage
import dev.faktor.shared.NativeRequests
import dev.faktor.shared.parseNativeAgents
import dev.faktor.shared.parseNativeModelCatalog
import dev.faktor.shared.parseNativePermissionList
import dev.faktor.shared.parseNativeProviders
import dev.faktor.shared.parseNativeSessionList
import dev.faktor.shared.parseNativeTerminalEventPage
import dev.faktor.shared.parseNativeTerminalOutput
import dev.faktor.shared.parseNativeTerminalPage
import dev.faktor.shared.parseNativeTerminalSpawned
import dev.faktor.shared.parseNativeTournament
import dev.faktor.shared.view
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.File
import java.io.InputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.nio.file.Files
import java.nio.file.Paths
import java.security.MessageDigest
import java.util.Collections

object JetBrainsParitySmoke {

    private var failures = 0

    @JvmStatic
    fun main(args: Array<String>) {
        step("pin: upstream JetBrains 7.1.2 tree matches ui/upstream.json") {
            verifyUpstreamPin()
        }
        step("task mode: composer contract + strict request bodies") {
            taskModeCanned()
        }
        step("agent tree: blockers, presentation, pixel identity") {
            agentTreeCanned()
        }
        step("permissions: pending list, replies, unavailable state") {
            permissionsCanned()
        }
        step("terminal: rows, spawn body, events, output") {
            terminalCanned()
        }
        step("review/tournament: review verdicts + decide gating") {
            reviewCanned()
        }
        step("evidence navigation: refs, retrieval selector, transcript jump") {
            evidenceCanned()
        }
        step("settings: registry view + mutation modes") {
            settingsCanned()
        }
        step("provider selection: per-provider model join") {
            providerSelectionCanned()
        }
        step("history: durable session list + open") {
            historyCanned()
        }

        if (args.isEmpty()) {
            println(
                if (failures == 0) {
                    "JETBRAINS PARITY SMOKE PASS (canned + pin only: no daemon binary argument)"
                } else {
                    "JETBRAINS PARITY SMOKE FAIL ($failures)"
                }
            )
            kotlin.system.exitProcess(if (failures == 0) 0 else 1)
        }

        step("fake daemon: panel end-to-end (task/agents/permissions/terminal/review/evidence/settings/history)") {
            fakeDaemonSuite()
        }
        step("fake daemon: reconnect resumes the SSE cursor") {
            fakeReconnectSuite()
        }
        step("real daemon: history/task/permissions/terminal/restart/reconnect") {
            realDaemonSuite(args[0])
        }

        println(if (failures == 0) "JETBRAINS PARITY SMOKE PASS" else "JETBRAINS PARITY SMOKE FAIL ($failures)")
        kotlin.system.exitProcess(if (failures == 0) 0 else 1)
    }

    // ------------------------------------------------------- 1. upstream pin

    /**
     * Re-hashes every vendored JetBrains 7.1.2 file against
     * `ui/upstream.json.jetbrains_712.file_hashes` (offline). A missing,
     * modified or unexpected file is a hard failure, and the Faktor patch
     * set paths must exist. This is the offline twin of
     * `scripts/vendor-upstream.sh --check` and needs no network.
     */
    private fun verifyUpstreamPin() {
        val root = resolveRepoRoot()
        val manifestFile = File(root, "ui/upstream.json")
        assertTrue(manifestFile.isFile, "missing ${manifestFile.path}")
        val manifest = JsonCodec.parse(manifestFile.readText(Charsets.UTF_8)).view("ui/upstream.json")
        val pin = manifest.field("jetbrains_712")
        assertEquals(
            "436ff09e649bd0866c84bd9f98933a74cad2d25c",
            pin.field("commit").string(),
            "pinned JetBrains commit"
        )
        assertEquals("7.1.2", pin.field("version").string(), "pinned JetBrains version")
        assertEquals("sha256", pin.field("hashAlgorithm").string(), "hash algorithm")
        val vendoredRoot = pin.field("vendoredRoot").string()
        val hashedRoot = pin.field("hashedRoot").string()
        val hashes = (pin.field("file_hashes").value as? JsonValue.Obj)
            ?: fail("jetbrains_712.file_hashes must be an object")
        val expected = pin.field("fileCount").long()
        assertEquals(
            expected, hashes.fields.size.toLong(),
            "file_hashes count must equal fileCount"
        )
        var bytes = 0L
        for ((rel, value) in hashes.fields) {
            val hash = (value as? JsonValue.Str)?.value ?: fail("hash for $rel must be a string")
            val file = File(root, "$hashedRoot/$rel")
            if (!file.isFile) fail("missing pinned file: $hashedRoot/$rel")
            bytes += file.length()
            val got = sha256Hex(file.readBytes())
            if (got != hash) fail("hash mismatch: $hashedRoot/$rel (expected $hash, got $got)")
        }
        assertEquals(pin.field("totalBytes").long(), bytes, "totalBytes must match the tree")
        // The hash keys are `kilo-jetbrains/**` under hashedRoot; walk that
        // exact subtree so Faktor metadata (NOTICE.md, LICENSES/) is not
        // mistaken for a vendored upstream file.
        val treePrefix = pin.field("upstreamPath").string().substringAfterLast('/')
        val extra = ArrayList<String>()
        walkFiles(File(root, "$hashedRoot/$treePrefix"), treePrefix, extra)
        assertEquals(expected, extra.size.toLong(), "vendored tree file count")
        for (rel in extra) {
            if (!hashes.fields.containsKey(rel)) fail("unexpected file in pinned tree: $rel")
        }
        val licenseObj = pin.field("licenseFiles").value as? JsonValue.Obj
            ?: fail("jetbrains_712.licenseFiles must be an object")
        for ((rel, value) in licenseObj.fields) {
            val hash = (value as? JsonValue.Str)?.value ?: fail("license hash for $rel")
            val file = File(root, "$vendoredRoot/$rel")
            if (!file.isFile) fail("missing license file: $vendoredRoot/$rel")
            if (sha256Hex(file.readBytes()) != hash) fail("license hash mismatch: $rel")
        }
        for (patch in pin.field("faktorPatchSet").array()) {
            val faktor = patch.field("faktor").string()
            assertTrue(File(root, faktor).isFile, "patch-set path must exist: $faktor")
        }
        println("UPSTREAM PIN PASS: commit ${pin.field("commit").string()} " +
            "${hashes.fields.size} files, $bytes bytes, sha256 verified")
    }

    private fun resolveRepoRoot(): File {
        val prop = System.getProperty("faktor.repo.root")
        if (prop != null && prop.isNotEmpty()) {
            val root = File(prop).absoluteFile
            assertTrue(File(root, "ui/upstream.json").isFile, "faktor.repo.root has no ui/upstream.json: $root")
            return root
        }
        var dir: File? = File(".").absoluteFile
        var guard = 0
        while (dir != null && guard < 8) {
            if (File(dir, "ui/upstream.json").isFile) return dir
            dir = dir.parentFile
            guard++
        }
        fail("cannot locate the repository root (ui/upstream.json); pass -Dfaktor.repo.root")
    }

    private fun walkFiles(dir: File, prefix: String, out: MutableList<String>) {
        val children = dir.listFiles() ?: return
        for (child in children) {
            val rel = if (prefix.isEmpty()) child.name else "$prefix/${child.name}"
            if (child.isDirectory) {
                walkFiles(child, rel, out)
            } else if (child.isFile) {
                out.add(rel)
            } else {
                fail("symlink not allowed in the pinned tree: $rel")
            }
        }
    }

    private fun sha256Hex(bytes: ByteArray): String {
        val digest = MessageDigest.getInstance("SHA-256").digest(bytes)
        val sb = StringBuilder(digest.size * 2)
        for (byte in digest) {
            val value = byte.toInt() and 0xff
            sb.append(Character.forDigit(value ushr 4, 16))
            sb.append(Character.forDigit(value and 0x0f, 16))
        }
        return sb.toString()
    }

    // --------------------------------------------------------- 2. task mode

    private fun taskModeCanned() {
        // The strict request bodies the Task composer emits.
        assertEquals(
            "{\"goal\":\"g\",\"criteria\":[\"c\"],\"mutation_mode\":\"shadow\"}",
            NativeRequests.startTaskRun("g", listOf("c"), mutationMode = "shadow")
        )
        assertEquals(
            "{\"goal\":\"g\",\"criteria\":[\"c\"],\"files\":[\"/tmp/a.rs\"]," +
                "\"work_items\":[{\"id\":\"main\",\"kind\":\"Implementation\"," +
                "\"summary\":\"g\",\"ownership\":\"isolated_worktree\"}]," +
                "\"completion_contract\":{\"include_commit\":true,\"include_push\":false," +
                "\"include_pr\":true}}",
            NativeRequests.startTaskRun(
                "g",
                listOf("c"),
                files = listOf("/tmp/a.rs"),
                completionContract = NativeCompletionContract(true, false, true)
            )
        )
        // The panel's Task tab is the only place a contract is checked.
        val panel = FaktorChatPanel(
            FaktorFrontendService(
                Paths.get("unused-binary"),
                Paths.get(System.getProperty("java.io.tmpdir"), "faktor-parity-task")
            )
        )
        try {
            assertTrue(panel.tabTitles().contains("Task"), panel.tabTitles().toString())
            assertTrue(panel.tabTitles().contains("Permissions"), panel.tabTitles().toString())
            assertTrue(panel.tabTitles().contains("Terminal"), panel.tabTitles().toString())
            assertTrue(panel.tabTitles().contains("Settings"), panel.tabTitles().toString())
            assertTrue(panel.tabTitles().contains("History"), panel.tabTitles().toString())
            assertEquals(null, panel.completionContractFromControls())
            panel.completionCommit.isSelected = true
            panel.completionPr.isSelected = true
            assertEquals(
                NativeCompletionContract(includeCommit = true, includePush = false, includePr = true),
                panel.completionContractFromControls()
            )
        } finally {
            panel.shutdown()
        }
    }

    // --------------------------------------------------------- 3. agent tree

    private fun agentTreeCanned() {
        val agents = parseNativeAgents(AGENTS_JSON)
        assertEquals(2, agents.size)
        val child = agents[1]
        assertEquals("Blocked", child.state)
        assertEquals("permission", child.blocker?.kind)
        assertEquals("background", child.presentation)
        // Background children are tucked after foreground children.
        val foreground = child.copy(agentId = "child-2", presentation = "foreground")
        val model = TaskTree.build(agents = listOf(child, foreground))
        assertEquals(2, model.children.size)
        assertEquals(listOf("child-2", "child-1"), model.children.map { it.childId })
        assertEquals(true, model.children[1].background)
        assertEquals(2, model.blockers.size)
        val tree = TaskTreePanel()
        tree.update(model)
        val label = tree.childLabel(model.children[1])
        assertTrue(label.contains("child-1"), label)
        assertTrue(label.contains("[Blocked]"), label)
        assertTrue(label.contains("blocker=permission"), label)
        assertTrue(label.contains("provider=alpha"), label)
        assertTrue(label.contains("(background)"), label)
        // Pixel identity is deterministic across instances (same FNV-1a hash
        // the VS Code pixel agents use) and every state animates.
        assertEquals(PixelAgents.avatar("child-1"), PixelAgents.avatar("child-1"))
        assertEquals(PixelState.BLOCKED, PixelAgents.stateOf("Blocked"))
        assertEquals(PixelState.RUNNING, PixelAgents.stateOf("Running"))
        assertTrue(PixelAgents.avatar("child-1").hue in 0..359, "hue in range")
        // Presentation transitions ride the native ack shape.
        assertEquals(
            "{\"state\":\"background\"}",
            NativeRequests.changePresentation("background")
        )
    }

    // -------------------------------------------------------- 4. permissions

    private fun permissionsCanned() {
        val permissions = parseNativePermissionList(PERMISSION_LIST_JSON)
        assertEquals(1, permissions.size)
        val panel = PermissionsPanel()
        var repliedId: String? = null
        var repliedDecision: String? = null
        var refreshes = 0
        panel.setListener(object : PermissionsPanel.Listener {
            override fun onPermissionReply(permission: dev.faktor.shared.NativePermissionEntry, decision: String) {
                repliedId = permission.id
                repliedDecision = decision
            }

            override fun onRefresh() {
                refreshes++
            }
        })
        // Before any data the buttons are inert; an unavailable route is
        // never rendered as an empty pending list.
        assertEquals(false, panel.allowEnabled())
        panel.setUnavailable("no permission route (status 404 not_found)")
        assertEquals(false, panel.available())
        assertTrue(panel.headerText().contains("unavailable"), panel.headerText())
        val permission = permissions[0]
        panel.update(permissions)
        assertEquals(true, panel.available())
        assertEquals(1, panel.count())
        assertEquals(permission, panel.selected())
        assertTrue(panel.unitLabel(0).contains("capability=shell"), panel.unitLabel(0))
        assertTrue(panel.detailText().contains("\"tool\":\"bash\""), panel.detailText())
        assertEquals(true, panel.allowEnabled())
        assertEquals(true, panel.denyEnabled())
        panel.submitReply("deny")
        assertEquals("7", repliedId)
        assertEquals("deny", repliedDecision)
        // The reply body is the strict daemon DTO.
        assertEquals(
            "{\"permission_id\":\"7\",\"decision\":\"allow\"}",
            NativeRequests.permissionReply("7", "allow")
        )
    }

    // ----------------------------------------------------------- 5. terminal

    private fun terminalCanned() {
        val page = parseNativeTerminalPage(TERMINALS_JSON)
        assertEquals("7", page.sessionId)
        assertEquals(1, page.terminals.size)
        assertEquals("5", page.terminals[0].id)
        assertEquals(true, page.terminals[0].alive)
        assertEquals("3", page.terminals[0].taskId)
        assertEquals(null, page.terminals[0].agentId)
        assertEquals(1L, page.unowned)
        val events = parseNativeTerminalEventPage(TERMINAL_EVENTS_JSON)
        assertEquals(1, events.events.size)
        assertEquals("created", events.events[0].type)
        assertEquals(false, events.hasMore)
        assertEquals("6", parseNativeTerminalSpawned(TERMINAL_SPAWNED_JSON).ptyId)
        val output = parseNativeTerminalOutput("6", TERMINAL_OUTPUT_JSON)
        assertEquals("6", output.ptyId)
        assertEquals(true, output.alive)
        assertTrue(output.output.contains("parity-output"), output.output)

        val panel = TerminalPanel()
        var spawnCommand: String? = null
        var spawnArgs: List<String>? = null
        var spawnCwd: String? = null
        var outputId: String? = null
        var refreshes = 0
        panel.setListener(object : TerminalPanel.Listener {
            override fun onRefresh() {
                refreshes++
            }

            override fun onSpawn(command: String, args: List<String>, cwd: String?) {
                spawnCommand = command
                spawnArgs = args
                spawnCwd = cwd
            }

            override fun onOutput(ptyId: String) {
                outputId = ptyId
            }
        })
        panel.setUnavailable("terminal read refused (status 404 not_found)")
        assertEquals(false, panel.available())
        panel.update(page)
        assertEquals(true, panel.available())
        assertEquals(1, panel.terminalCount())
        assertTrue(panel.terminalLabel(0).contains("pty 5"), panel.terminalLabel(0))
        assertTrue(panel.terminalLabel(0).contains("task=3"), panel.terminalLabel(0))
        assertTrue(panel.eventsText().contains("unowned daemon-level rows: 1"), panel.eventsText())
        panel.setEvents(events)
        assertTrue(panel.eventsText().contains("#1 created"), panel.eventsText())
        panel.select(0)
        assertEquals("5", panel.selectedTerminalId())
        panel.setComposerFields("bash", "-lc echo-parity", "/tmp")
        panel.submitSpawn()
        assertEquals("bash", spawnCommand)
        assertEquals(listOf("-lc", "echo-parity"), spawnArgs)
        assertEquals("/tmp", spawnCwd)
        // Output routing uses the selected pty id.
        panel.setComposerFields("bash", "", "")
        assertEquals("5", panel.selectedTerminalId())
        panel.setOutput(output)
        assertTrue(panel.outputText().contains("pty 6 alive=true"), panel.outputText())
        // The strict spawn body: exactly the daemon's accepted fields.
        assertEquals(
            "{\"command\":\"bash\",\"args\":[\"-lc\",\"echo parity\"],\"cwd\":\"/tmp\"," +
                "\"rows\":24,\"cols\":80}",
            NativeRequests.spawnTerminal("bash", listOf("-lc", "echo parity"), "/tmp", 24L, 80L)
        )
        assertEquals(
            "{\"command\":\"bash\"}",
            NativeRequests.spawnTerminal("bash")
        )
        // Unknown extra bytes (extra arg) still produce one strict body.
        assertEquals(
            "{\"command\":\"bash\",\"args\":[]}",
            NativeRequests.spawnTerminal("bash", emptyList())
        )
    }

    // -------------------------------------------------- 6. review/tournament

    private fun reviewCanned() {
        val view = TaskTree.tournamentView(parseNativeTournament(TOURNAMENT_JSON))
        val panel = TournamentPanel()
        panel.setSummaries(dev.faktor.shared.parseNativeTournamentSummaries(TOURNAMENTS_LIST_JSON))
        panel.setTournament(view)
        assertEquals(2, panel.candidateCount())
        assertEquals(false, panel.decideEnabled(), "a decided tournament exposes no decide")
        // Review verdicts (rank + reviewer) are surfaced per candidate.
        assertEquals("clean", view.candidates[0].reviewRank)
        assertEquals("rev-1", view.candidates[0].reviewer)
        assertEquals(true, view.candidates[0].winner)
        assertEquals(null, view.candidates[1].reviewRank)
        val running = view.copy(
            candidates = view.candidates.map { it.copy(state = "done") },
            winner = null,
            state = "open"
        )
        panel.setTournament(running)
        assertEquals(true, panel.decideEnabled(), "all candidates settled => decide is available")
        assertEquals(true, panel.abortEnabled())
        val pending = running.copy(
            candidates = running.candidates.mapIndexed { index, candidate ->
                if (index == 1) candidate.copy(state = "running") else candidate
            }
        )
        panel.setTournament(pending)
        assertEquals(false, panel.decideEnabled(), "one running candidate gates decide")
    }

    // ------------------------------------------------------- 7. evidence nav

    private fun evidenceCanned() {
        val refs = listOf(
            EvidenceRefs.parse("evidence:41"),
            EvidenceRefs.parse("see evidence/42 here")
        )
        assertEquals(41L, refs[0].id)
        assertEquals(42L, refs[1].id)
        val panel = EvidenceNavigatorPanel()
        var retrievedId: Long? = null
        var selector: String? = null
        var jumpedSeq: Long? = null
        panel.setListener(object : EvidenceNavigatorPanel.Listener {
            override fun onRetrieve(evidenceId: Long, selectorJson: String) {
                retrievedId = evidenceId
                selector = selectorJson
            }

            override fun onMessageSelected(seq: Long) {
                jumpedSeq = seq
            }
        })
        panel.setEvidence(refs)
        panel.setMessages(
            listOf(
                NativeMessage(seq = 1, id = 1, role = "user", createdMs = 1, text = "hi"),
                NativeMessage(seq = 2, id = 2, role = "assistant", createdMs = 2, text = "hello")
            )
        )
        assertEquals(2, panel.evidenceCount())
        assertEquals(2, panel.messageCount())
        panel.selectEvidence(refs[0])
        assertEquals(refs[0], panel.selectedEvidence())
        assertEquals("{\"selector\":\"all\"}", panel.selectorJson())
        panel.showRetrieval(41L, "evidence:41 tool output", 12L, false)
        panel.showError(42L, "unknown evidence id")
        panel.showTranscriptSlice("child child-1", "#1 user: hi")
        // The retrieval request body is the strict selector DTO.
        assertEquals(
            "{\"selector\":\"search\",\"query\":\"needle\",\"max_hits\":3}",
            NativeRequests.evidenceSelectorSearch("needle", 3L)
        )
    }

    // --------------------------------------------------------- 8. settings

    private fun settingsCanned() {
        val providers = parseNativeProviders(PROVIDERS_JSON)
        assertEquals(2, providers.size)
        assertEquals("alpha", providers[0].instanceId)
        assertEquals("openai", providers[0].family)
        assertEquals(1, providers[0].models.size)
        assertEquals(true, providers[0].models[0].reasoning)
        assertEquals(true, providers[0].runtimeContextLimitSupported)
        val panel = SettingsPanel()
        panel.setDaemonInfo("/bin/faktor-cli", "/tmp/data", "1.2.3", "http://127.0.0.1:9")
        assertTrue(panel.daemonText().contains("/bin/faktor-cli"), panel.daemonText())
        assertEquals(listOf("default", "shadow", "direct_compat"), panel.mutationModes())
        assertEquals(null, panel.mutationMode())
        panel.selectMutationMode("direct_compat")
        assertEquals("direct_compat", panel.mutationMode())
        panel.setProviders(providers)
        assertEquals(2, panel.providerCount())
        assertEquals("alpha", panel.selectedProvider())
        assertEquals("m", panel.selectedModel())
        assertEquals(1, panel.modelCount())
        assertTrue(panel.providerLabel(0).contains("openai"), panel.providerLabel(0))
        assertTrue(panel.statusText().contains("selected=alpha/m"), panel.statusText())
        // Selecting another provider narrows the model combo to its models.
        panel.selectProvider("beta")
        assertEquals("beta", panel.selectedProvider())
        assertEquals("n", panel.selectedModel())
        panel.setUnavailable("provider read failed: daemon down")
        assertEquals(false, panel.available())
        assertTrue(panel.statusText().contains("unavailable"), panel.statusText())
    }

    // -------------------------------------------------- 9. provider selection

    private fun providerSelectionCanned() {
        // Two providers expose the SAME model id with different capability
        // sets: the (provider, model) pair is the only safe join key.
        val catalog = parseNativeModelCatalog(DUAL_MODELS_JSON)
        val panel = SettingsPanel()
        panel.setCatalog(catalog)
        assertEquals(2, panel.providerCount())
        assertEquals("alpha", panel.selectedProvider())
        assertEquals("m", panel.selectedModel())
        panel.selectModel("m")
        // The panel reports the selected pair, never a guessed one.
        assertEquals("alpha/m", "${panel.selectedProvider()}/${panel.selectedModel()}")
        panel.selectProvider("beta")
        assertEquals("beta/m", "${panel.selectedProvider()}/${panel.selectedModel()}")
        // A provider catalog with no rows records an honest empty state.
        panel.setCatalog(emptyList())
        assertEquals(0, panel.providerCount())
        assertEquals(null, panel.selectedProvider())
        val child = parseNativeAgents(AGENTS_JSON)[1]
        val model = TaskTree.build(
            agents = listOf(child.copy(provider = "alpha"), child.copy(agentId = "child-b", provider = "beta")),
            catalog = catalog
        )
        assertEquals(true, model.children[0].reasoning)
        assertEquals(false, model.children[1].reasoning)
    }

    // ---------------------------------------------------------- 10. history

    private fun historyCanned() {
        val sessions = parseNativeSessionList(SESSIONS_JSON)
        assertEquals(2, sessions.size)
        val panel = HistoryPanel()
        var openedId: String? = null
        var restarts = 0
        var reconnects = 0
        var refreshes = 0
        panel.setListener(object : HistoryPanel.Listener {
            override fun onOpenSession(sessionId: String) {
                openedId = sessionId
            }

            override fun onRestart() {
                restarts++
            }

            override fun onReconnect() {
                reconnects++
            }

            override fun onRefresh() {
                refreshes++
            }
        })
        panel.setUnavailable("history read refused (status 503)")
        assertEquals(false, panel.available())
        panel.update(sessions, "7")
        assertEquals(true, panel.available())
        assertEquals(2, panel.count())
        assertTrue(panel.label(0).contains("(current)"), panel.label(0))
        assertTrue(panel.statusText().contains("current=7"), panel.statusText())
        panel.select(1)
        assertEquals("8", panel.selectedId())
        panel.submitOpen()
        assertEquals("8", openedId)
        assertEquals(true, panel.openEnabled())
        assertEquals(true, panel.restartEnabled())
        assertEquals(true, panel.reconnectEnabled())
        panel.setConnection("connected: http://127.0.0.1:9", "open", 42L, "7")
        assertTrue(panel.streamText().contains("cursor=42"), panel.streamText())
        // The restart/reconnect controls are always available; the reconnect
        // control needs a selected session.
        panel.setConnection("stopped", "off", 0L, null)
    }

    // ------------------------------------------------------ fake daemon e2e

    private fun fakeDaemonSuite() {
        val daemon = ParityFakeDaemon()
        registerRoutes(daemon)
        daemon.start()
        val process = ProcessBuilder(javaBinary(), "-version").start()
        val connection = BackendConnection(
            daemon.server.localPort, PARITY_PASSWORD, process, StdoutSink(process)
        )
        val service = FaktorFrontendService(
            Paths.get("unused"),
            Paths.get(System.getProperty("java.io.tmpdir"), "faktor-parity-fake")
        )
        var panel: FaktorChatPanel? = null
        try {
            service.attachConnection(connection, stopAction = { process.destroyForcibly() })
            val created = service.createSession("alpha", "m", title = "parity")
            assertEquals("7", created.id)
            service.watchSession("7", 0)
            panel = FaktorChatPanel(service)
            assertTrue(panel.refreshNowForTest(), "one refresh cycle must finish")

            // history: the durable session listing, current marked.
            assertEquals(2, panel.historyView().count())
            assertTrue(panel.historyView().label(0).contains("[ready]"), panel.historyView().label(0))
            assertTrue(panel.historyView().streamText().contains("session=7"), panel.historyView().streamText())

            // provider selection: the registry view drives the combos.
            assertEquals(2, panel.settingsView().providerCount())
            assertEquals("alpha", panel.settingsView().selectedProvider())
            assertEquals("m", panel.settingsView().selectedModel())

            // permissions: pending list rendered; reply posts the strict body.
            assertEquals(1, panel.permissionsView().count())
            assertTrue(
                panel.permissionsView().unitLabel(0).contains("capability=shell"),
                panel.permissionsView().unitLabel(0)
            )
            panel.permissionsView().submitReply("deny")
            await("permission reply routed") {
                daemon.lastRequest("POST", "/permission/reply") != null
            }
            assertEquals(
                "{\"permission_id\":\"7\",\"decision\":\"deny\"}",
                daemon.lastRequest("POST", "/permission/reply")!!.body
            )

            // terminal: session-owned rows, spawn body, output snapshot.
            assertEquals(1, panel.terminalView().terminalCount())
            assertTrue(panel.terminalView().terminalLabel(0).contains("task=3"), panel.terminalView().terminalLabel(0))
            panel.terminalView().setComposerFields("bash", "-lc echo-parity", "/tmp")
            panel.terminalView().submitSpawn()
            await("terminal spawn routed") {
                daemon.lastRequest("POST", "/native/session/7/terminal") != null
            }
            val spawnBody = daemon.lastRequest("POST", "/native/session/7/terminal")!!.body
            assertTrue(spawnBody.contains("\"command\":\"bash\""), spawnBody)
            assertTrue(spawnBody.contains("\"args\":[\"-lc\",\"echo-parity\"]"), spawnBody)
            assertTrue(spawnBody.contains("\"cwd\":\"/tmp\""), spawnBody)
            val output = service.terminalOutput("6")
            assertEquals("6", output.ptyId)
            assertTrue(output.output.contains("parity-output"), output.output)

            // agent tree: children + blockers rendered from the fake frames.
            val tree = panel.taskTreeView().model() ?: fail("the tree must be populated")
            assertEquals(1, tree.children.size)
            assertEquals("child-1", tree.children[0].childId)
            assertEquals("permission", tree.children[0].blocker?.kind)
            assertEquals(1, panel.blockersView().blockerCount())

            // task mode: one request carries criteria + mutation default +
            // completion contract + the attachment set.
            panel.settingsView().selectMutationMode("direct_compat")
            panel.completionCommit.isSelected = true
            val attachment = Files.createTempFile("faktor-parity-attach-", ".txt")
            panel.attachmentsView().addFiles(listOf(attachment.toString()))
            panel.setTaskFieldsForTest("parity goal", "criterion A, criterion B")
            panel.submitTaskForTest()
            await("task run routed") {
                daemon.lastRequest("POST", "/native/session/7/task-runs") != null
            }
            val taskBody = daemon.lastRequest("POST", "/native/session/7/task-runs")!!.body
            assertTrue(taskBody.contains("\"goal\":\"parity goal\""), taskBody)
            assertTrue(taskBody.contains("\"criteria\":[\"criterion A\",\"criterion B\"]"), taskBody)
            assertTrue(taskBody.contains("\"mutation_mode\":\"direct_compat\""), taskBody)
            assertTrue(taskBody.contains("\"completion_contract\":{\"include_commit\":true"), taskBody)
            assertTrue(taskBody.contains(attachment.toString()), taskBody)
            await("task refresh after start") {
                panel.taskTreeView().model()?.steps?.isNotEmpty() ?: false
            }

            // history open: selecting the other durable session switches the
            // current session and reopens the SSE stream.
            panel.historyView().select(1)
            panel.historyView().submitOpen()
            await("session 8 opened") { service.currentSessionId() == "8" }

            // new session uses the Settings provider selection (alpha/m),
            // never the stale composer defaults.
            panel.newSessionForTest()
            await("session created from selection") {
                val request = daemon.lastRequest("POST", "/session/create")
                request != null && daemon.requestCount("POST", "/session/create") >= 2
            }
            val createBody = daemon.lastRequest("POST", "/session/create")!!.body
            assertTrue(createBody.contains("\"provider\":\"alpha\""), createBody)
            assertTrue(createBody.contains("\"model\":\"m\""), createBody)
        } finally {
            if (panel != null) panel.shutdown()
            service.stop()
            daemon.stop()
        }
    }

    // ------------------------------------------------- fake daemon: reconnect

    private fun fakeReconnectSuite() {
        val daemon = ParityFakeDaemon()
        registerRoutes(daemon)
        daemon.start()
        val process = ProcessBuilder(javaBinary(), "-version").start()
        val connection = BackendConnection(
            daemon.server.localPort, PARITY_PASSWORD, process, StdoutSink(process)
        )
        val service = FaktorFrontendService(
            Paths.get("unused"),
            Paths.get(System.getProperty("java.io.tmpdir"), "faktor-parity-reconnect")
        )
        try {
            service.attachConnection(connection, stopAction = { process.destroyForcibly() })
            service.createSession("alpha", "m", title = "reconnect")
            service.watchSession("7", 0)
            await("initial SSE frames delivered") { service.streamCursor() >= 2L }
            val before = service.streamCursor()
            val resumed = service.reconnectStream()
            assertEquals(before, resumed, "reconnect resumes from the delivered cursor")
            await("resumed stream delivers the next frame") { service.streamCursor() >= 3L }
            val resumedRequests = daemon.requests
                .filter { it.path == "/api/session/7/events" && it.query["events_after"] == "2" }
            assertTrue(
                resumedRequests.isNotEmpty(),
                "the second SSE connection must resume at events_after=2"
            )
        } finally {
            service.stop()
            daemon.stop()
        }
    }

    // ----------------------------------------------------- real daemon suite

    private fun realDaemonSuite(binaryPath: String) {
        val binary = Paths.get(binaryPath)
        val dataDir = Files.createTempDirectory("faktor-parity-real-")
        val workspace = Files.createTempDirectory("faktor-parity-workspace-")
        Files.write(Paths.get(workspace.toString(), "seed.txt"), "parity".toByteArray())
        val service = FaktorFrontendService(binary, dataDir)
        var sessionId: String? = null
        try {
            step("real: start + create session") {
                val health = service.start()
                assertTrue(health.ok, "health ok=false")
                sessionId = service.createSession(
                    "default", "default", workspace.toString(), "parity"
                ).id
            }
            val sid = sessionId ?: fail("no session id")
            step("real: history lists the session") {
                val sessions = service.listSessions()
                assertTrue(sessions.any { it.id == sid }, "session $sid must be listed")
            }
            step("real: providers + models routes answer") {
                service.providers()
                service.modelCatalog()
            }
            step("real: task-run accepts goal/criteria/mutation mode") {
                val started = service.startTaskRun(
                    "parity real goal",
                    listOf("criterion A"),
                    mutationMode = "shadow"
                )
                assertTrue(started.runId.isNotEmpty(), "no run id")
            }
            step("real: permissions route is served") {
                service.permissions()
            }
            step("real: terminal spawn + session listing + events") {
                val spawned = service.spawnTerminal(
                    "sh", listOf("-c", "sleep 5; echo parity"), null
                )
                assertTrue(spawned.ptyId.isNotEmpty(), "no pty id")
                val page = service.terminals()
                assertEquals(sid, page.sessionId)
                assertTrue(
                    page.terminals.any { it.id == spawned.ptyId },
                    "the spawned terminal must be owned by the session"
                )
                val events = service.terminalEvents(limit = 64L)
                assertTrue(
                    events.events.any { it.ptyId == spawned.ptyId },
                    "the spawn must have a durable lifetime event"
                )
                val output = service.terminalOutput(spawned.ptyId)
                assertEquals(spawned.ptyId, output.ptyId)
            }
            step("real: restart preserves the durable session and cursor") {
                service.watchSession(sid, 0)
                await("stream opens before restart") {
                    service.streamStatus() == "open" || service.streamStatus() == "retrying"
                }
                val health = service.restart()
                assertTrue(health.ok, "restarted daemon not healthy")
                assertEquals(sid, service.currentSessionId(), "session must survive restart")
                assertTrue(service.isRunning(), "service must be running after restart")
            }
            step("real: reconnect at the current cursor") {
                val cursor = service.reconnectStream()
                assertTrue(cursor != null, "a session must be selected for reconnect")
            }
        } finally {
            service.stop()
            dataDir.toFile().deleteRecursively()
            workspace.toFile().deleteRecursively()
        }
    }

    // ------------------------------------------------------------- plumbing

    private fun registerRoutes(daemon: ParityFakeDaemon) {
        daemon.on("GET", "/native/health") { _, response -> response.json(200, HEALTH_JSON) }
        daemon.on("GET", "/native/ready") { _, response -> response.json(200, READY_JSON) }
        daemon.on("POST", "/session/create") { _, response -> response.json(200, CREATED_JSON) }
        daemon.on("GET", "/session/list") { _, response -> response.json(200, SESSIONS_JSON) }
        daemon.on("GET", "/models") { _, response -> response.json(200, MODELS_JSON) }
        daemon.on("GET", "/native/providers") { _, response -> response.json(200, PROVIDERS_JSON) }
        daemon.on("GET", "/native/usage") { _, response -> response.json(200, USAGE_JSON) }
        daemon.on("GET", "/session/7/projection") { _, response -> response.json(200, PROJECTION_JSON) }
        daemon.on("GET", "/native/messages") { _, response -> response.json(200, MESSAGES_JSON) }
        daemon.on("GET", "/native/session/7/tasks") { _, response -> response.json(200, TASK_VIEWS_JSON) }
        daemon.on("GET", "/native/session/7/task-runs") { _, response -> response.json(200, TASK_RUNS_JSON) }
        daemon.on("POST", "/native/session/7/task-runs") { _, response ->
            response.json(200, TASK_RUN_STARTED_JSON)
        }
        daemon.on("GET", "/native/session/7/tasks/3/verification") { _, response ->
            response.json(200, TASK_VERIFICATION_JSON)
        }
        daemon.on("GET", "/native/session/7/verification") { _, response ->
            response.json(200, VERIFICATION_JSON)
        }
        daemon.on("GET", "/native/session/7/usage") { _, response -> response.json(200, SESSION_USAGE_JSON) }
        daemon.on("GET", "/native/session/9/usage") { _, response -> response.json(200, SESSION_USAGE_JSON) }
        daemon.on("GET", "/native/session/7/tournaments") { _, response ->
            response.json(200, TOURNAMENTS_LIST_JSON)
        }
        daemon.on("GET", "/native/session/7/board") { _, response -> response.json(200, BOARD_PAGE_JSON) }
        daemon.on("GET", "/permission/list") { _, response -> response.json(200, PERMISSION_LIST_JSON) }
        daemon.on("POST", "/permission/reply") { _, response -> response.json(200, PERMISSION_ACK_JSON) }
        daemon.on("GET", "/native/agents") { _, response -> response.json(200, AGENTS_JSON) }
        daemon.on("GET", "/native/terminals") { _, response -> response.json(200, TERMINALS_JSON) }
        daemon.on("GET", "/native/session/7/terminal/events") { _, response ->
            response.json(200, TERMINAL_EVENTS_JSON)
        }
        daemon.on("POST", "/native/session/7/terminal") { _, response ->
            response.json(200, TERMINAL_SPAWNED_JSON)
        }
        daemon.on("GET", "/pty/6/output") { _, response -> response.json(200, TERMINAL_OUTPUT_JSON) }
        daemon.on("GET", "/pty/5/output") { _, response -> response.json(200, TERMINAL_OUTPUT_JSON) }
        for (session in listOf("7", "8")) {
            daemon.on("GET", "/api/session/$session/events") { request, response ->
                val after = request.query["events_after"]?.toLongOrNull() ?: 0L
                response.stream(200, "text/event-stream") { writer ->
                    if (after < 1L) {
                        writer.frame(1L, "message", "{\"event\":\"message\",\"kind\":\"message\",\"state\":\"one\"}")
                    }
                    if (after < 2L) {
                        writer.frame(2L, "message", "{\"event\":\"message\",\"kind\":\"message\",\"state\":\"two\"}")
                    }
                    if (after in 1L..2L) {
                        writer.frame(3L, "message", "{\"event\":\"message\",\"kind\":\"message\",\"state\":\"three\"}")
                    }
                }
            }
        }
    }

    private fun javaBinary(): String {
        val home = System.getProperty("java.home") ?: fail("java.home missing")
        val candidate = Paths.get(home, "bin", "java").toFile()
        return if (candidate.isFile) candidate.absolutePath else "java"
    }

    private fun await(what: String, timeoutMs: Long = 10_000L, condition: () -> Boolean) {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < deadline) {
            if (condition()) return
            Thread.sleep(25L)
        }
        fail("timed out waiting for: $what")
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

    // ----------------------------------------------------------- fixtures

    private const val PARITY_PASSWORD = "parity-token"

    private const val HEALTH_JSON = "{\"ok\":true,\"version\":\"fake-1\"}"

    private const val READY_JSON = "{\"ready\":true}"

    private const val CREATED_JSON = "{\"id\":\"7\",\"title\":\"parity\",\"created_ms\":1750000000000}"

    private const val SESSIONS_JSON = "{\"sessions\":[" +
        "{\"id\":\"7\",\"title\":\"parity\",\"provider\":\"alpha\",\"model\":\"m\"," +
        "\"state\":\"ready\"}," +
        "{\"id\":\"8\",\"title\":\"older\",\"provider\":\"beta\",\"model\":\"n\"," +
        "\"state\":\"ended\"}]}"

    private const val MODELS_JSON = "[" +
        "{\"provider\":\"alpha\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
        "\"tools\":true,\"parallelTools\":false,\"reasoning\":true,\"thinking\":false," +
        "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
        "\"streaming\":true,\"source\":\"conservativeDefault\"}," +
        "{\"provider\":\"beta\",\"model\":\"n\",\"context\":2000,\"maxOutput\":200," +
        "\"tools\":false,\"parallelTools\":false,\"reasoning\":false,\"thinking\":true," +
        "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
        "\"streaming\":true,\"source\":\"conservativeDefault\"}]"

    private const val PROVIDERS_JSON = "[" +
        "{\"instanceId\":\"alpha\",\"family\":\"openai\",\"models\":[" +
        "{\"model\":\"m\",\"context\":1000,\"maxOutput\":100,\"tools\":true," +
        "\"parallelTools\":false,\"reasoning\":true,\"thinking\":false,\"vision\":false," +
        "\"streaming\":true,\"source\":\"conservativeDefault\"}]," +
        "\"runtimeContextLimitSupported\":true," +
        "\"health\":{\"status\":\"registered\",\"note\":\"snapshot\"}}," +
        "{\"instanceId\":\"beta\",\"family\":\"anthropic\",\"models\":[" +
        "{\"model\":\"n\",\"context\":2000,\"maxOutput\":200,\"tools\":false," +
        "\"parallelTools\":false,\"reasoning\":false,\"thinking\":true,\"vision\":false," +
        "\"streaming\":true,\"source\":\"conservativeDefault\"}]," +
        "\"runtimeContextLimitSupported\":false," +
        "\"health\":{\"status\":\"registered\",\"note\":\"\"}}]"

    private const val DUAL_MODELS_JSON = "[" +
        "{\"provider\":\"alpha\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
        "\"tools\":true,\"parallelTools\":false,\"reasoning\":true,\"thinking\":false," +
        "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
        "\"streaming\":true,\"source\":\"conservativeDefault\"}," +
        "{\"provider\":\"beta\",\"model\":\"m\",\"context\":2000,\"maxOutput\":200," +
        "\"tools\":false,\"parallelTools\":false,\"reasoning\":false,\"thinking\":true," +
        "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
        "\"streaming\":true,\"source\":\"conservativeDefault\"}]"

    private const val PERMISSION_LIST_JSON = "{\"permissions\":[" +
        "{\"id\":\"7\",\"session_id\":\"9\",\"capability\":\"shell\"," +
        "\"detail\":{\"tool\":\"bash\"}}]}"

    private const val PERMISSION_ACK_JSON = "{\"ok\":true}"

    private const val TERMINALS_JSON = "{" +
        "\"sessionId\":\"7\",\"terminals\":[" +
        "{\"id\":\"5\",\"pid\":123,\"alive\":true,\"sessionId\":\"7\",\"taskId\":\"3\"," +
        "\"agentId\":null,\"operationId\":\"11\",\"spawnedMs\":1700}]," +
        "\"unowned\":1,\"note\":\"1 daemon-level PTY row carries no session ownership\"}"

    private const val TERMINAL_EVENTS_JSON = "{" +
        "\"sessionId\":\"7\",\"events\":[" +
        "{\"id\":1,\"type\":\"created\",\"ptyId\":\"5\",\"pid\":123,\"tsMs\":1700," +
        "\"sessionId\":\"7\"}]," +
        "\"hasMore\":false,\"nextCursor\":null}"

    private const val TERMINAL_SPAWNED_JSON = "{" +
        "\"ok\":true,\"ptyId\":\"6\",\"pid\":456,\"sessionId\":\"7\",\"taskId\":\"3\"," +
        "\"agentId\":null,\"operationId\":\"12\"}"

    private const val TERMINAL_OUTPUT_JSON =
        "{\"ok\":true,\"output\":\"parity-output\\nsecond line\\n\",\"alive\":true}"

    private const val TASK_RUN_STARTED_JSON =
        "{\"task_id\":3,\"run_id\":\"run-9\",\"state\":\"Running\"}"

    private const val TASK_RUNS_JSON = "[" +
        "{\"task_id\":3,\"run_id\":\"run-9\",\"mode\":\"in_session\",\"state\":\"Running\"," +
        "\"goal\":\"parity goal\",\"item_ids\":[\"main\"],\"model\":\"m\"}]"

    private const val TASK_VIEWS_JSON = "[" +
        "{\"goal\":\"parity goal\",\"constraints\":[],\"state\":\"in_progress\"," +
        "\"milestones\":{\"completed\":[],\"open\":[]}," +
        "\"decisions\":[],\"failures\":[],\"changedFiles\":[]," +
        "\"tests\":{\"run\":[],\"failed\":[]},\"preferences\":[],\"verification\":[]," +
        "\"progress\":null,\"budget\":null," +
        "\"plan\":[" +
        "{\"id\":\"step-1\",\"summary\":\"first\",\"state\":\"done\",\"depends_on\":[]}," +
        "{\"id\":\"step-2\",\"summary\":\"second\",\"state\":\"running\"," +
        "\"depends_on\":[\"step-1\"]}]," +
        "\"blockers\":[],\"evidenceRefs\":[],\"phase\":\"building\"}]"

    private const val TASK_VERIFICATION_JSON =
        "{\"sessionId\":\"7\",\"taskId\":\"3\",\"records\":[]}"

    private const val VERIFICATION_JSON = "{\"owed\":[],\"failedChecks\":[]}"

    private const val USAGE_JSON = "{" +
        "\"sessions\":1,\"totals\":{\"budget\":100,\"spent\":10},\"perSession\":[]," +
        "\"durable\":{\"sessionsWithCalls\":1," +
        "\"providerCalls\":{\"tokens\":10,\"prefixObservations\":0}," +
        "\"taskSpend\":{\"settledCostMicro\":2}," +
        "\"reservations\":{\"open\":{\"count\":0,\"predictedMicro\":0}," +
        "\"settled\":{\"count\":0,\"predictedMicro\":0,\"spentMicro\":0," +
        "\"providerReportedMicro\":0}," +
        "\"refunded\":{\"count\":0,\"predictedMicro\":0}," +
        "\"uncertain\":{\"count\":0,\"predictedMicro\":0},\"truncated\":false}}," +
        "\"truncated\":false}"

    private const val SESSION_USAGE_JSON = "{" +
        "\"sessionId\":\"7\",\"providerCalls\":{\"tokens\":10,\"prefixObservations\":[]}," +
        "\"prefixStability\":null,\"tasks\":[]}"

    private const val PROJECTION_JSON = "{" +
        "\"session\":{\"id\":\"7\",\"title\":\"parity\",\"provider\":\"alpha\",\"model\":\"m\"," +
        "\"lifecycle\":\"open\"}," +
        "\"state\":{\"machine\":\"idle\",\"label\":\"idle\",\"active\":false," +
        "\"terminal\":false}," +
        "\"activeModel\":{\"provider\":\"alpha\",\"model\":\"m\",\"variant\":null}," +
        "\"activeTool\":null,\"progress\":null,\"filesChanged\":[]," +
        "\"lastCheckpoint\":null,\"verification\":[]," +
        "\"contextUsage\":null,\"queued\":0}"

    private const val MESSAGES_JSON = "{" +
        "\"messages\":[{\"seq\":1,\"id\":1,\"role\":\"user\",\"createdMs\":1," +
        "\"parts\":[{\"kind\":\"text\",\"data\":{\"text\":\"hello parity\"}}]}]," +
        "\"hasMore\":false,\"nextBefore\":null}"

    private const val AGENTS_JSON = "[" +
        "{\"agent_id\":\"self-1\",\"kind\":\"self\",\"run_id\":\"run-9\"," +
        "\"session_id\":7,\"worktree_id\":1,\"goal\":\"ship it\",\"state\":\"Running\"," +
        "\"model\":\"m\",\"budget\":null,\"ownership\":\"self\",\"item_ids\":[\"main\"]," +
        "\"progress\":null,\"result\":null}," +
        "{\"agent_id\":\"child-1\",\"kind\":\"child\",\"run_id\":\"run-9\"," +
        "\"session_id\":9,\"worktree_id\":2,\"goal\":\"drive main step\",\"state\":\"Blocked\"," +
        "\"model\":\"m\",\"provider\":\"alpha\",\"budget\":1000,\"ownership\":\"Mutating\"," +
        "\"item_id\":\"main\",\"item_kind\":\"Implementation\"," +
        "\"blocker\":{\"kind\":\"permission\",\"reason\":\"shell call needs approval\"," +
        "\"dependency\":null,\"resolution\":\"allow the shell tool\"," +
        "\"last_progress_ms\":42}," +
        "\"capabilities\":[{\"cap\":\"ReadWorkspace\"}]," +
        "\"progress\":{\"lastOutputAt\":1,\"lastProgressAt\":2,\"lastOpCompletedAt\":3," +
        "\"inFlightOp\":null,\"silenceMs\":500,\"stallThresholdMs\":1000,\"stalled\":true}," +
        "\"result\":{\"summary\":\"main step output\",\"merge\":null}," +
        "\"presentation\":\"background\"}]"

    private const val TOURNAMENTS_LIST_JSON = "[]"

    private const val BOARD_PAGE_JSON = "{" +
        "\"board_id\":7,\"revision\":0,\"posts\":[]," +
        "\"next_before_revision\":null,\"has_more\":false}"

    private const val TOURNAMENT_JSON = "{" +
        "\"id\":\"t-1\",\"run_family\":\"run-7\",\"goal\":\"pick winner\"," +
        "\"criteria\":[{\"id\":\"c-1\",\"spec\":\"tests pass\"}]," +
        "\"candidates\":[" +
        "{\"child_id\":\"child-0\",\"worktree\":\"/tmp/w0\",\"base_revision\":\"abc\"," +
        "\"state\":\"done\",\"verification\":12,\"verification_pass\":true," +
        "\"review\":{\"rank\":\"clean\",\"reviewer\":\"rev-1\"}," +
        "\"cost_micro\":100,\"wall_ms\":1000}," +
        "{\"child_id\":\"child-1\",\"worktree\":\"/tmp/w1\",\"base_revision\":\"abc\"," +
        "\"state\":\"discarded\",\"verification\":null,\"verification_pass\":null," +
        "\"review\":null,\"cost_micro\":50,\"wall_ms\":900}]," +
        "\"winner\":\"child-0\",\"state\":\"decided\"}"
}

// ------------------------------------------------------------- fake daemon

private class ParityRequest(
    val method: String,
    val path: String,
    val query: Map<String, String>,
    val headers: Map<String, String>,
    val body: String
)

private class ParitySseWriter(private val out: BufferedOutputStream) {
    fun write(text: String) {
        out.write(text.toByteArray(Charsets.UTF_8))
        out.flush()
    }

    fun frame(id: Long, event: String, data: String) {
        write("id: $id\n")
        write("event: $event\n")
        write("data: $data\n\n")
    }
}

private class ParityResponse(private val out: BufferedOutputStream) {
    fun json(status: Int, body: String) {
        val bytes = body.toByteArray(Charsets.UTF_8)
        val head = "HTTP/1.1 $status X\r\nContent-Type: application/json\r\n" +
            "Content-Length: ${bytes.size}\r\nConnection: close\r\n\r\n"
        out.write(head.toByteArray(Charsets.UTF_8))
        out.write(bytes)
        out.flush()
    }

    fun stream(status: Int, contentType: String, block: (ParitySseWriter) -> Unit) {
        val head = "HTTP/1.1 $status X\r\nContent-Type: $contentType\r\n" +
            "Cache-Control: no-cache\r\nConnection: close\r\n\r\n"
        out.write(head.toByteArray(Charsets.UTF_8))
        out.flush()
        block(ParitySseWriter(out))
    }
}

/** Raw-socket fake of the native surface (127.0.0.1 only, one thread per connection). */
private class ParityFakeDaemon {
    val server = ServerSocket(0, 50, InetAddress.getByName("127.0.0.1"))
    val requests: MutableList<ParityRequest> = Collections.synchronizedList(ArrayList<ParityRequest>())
    private val handlers =
        Collections.synchronizedMap(HashMap<String, (ParityRequest, ParityResponse) -> Unit>())
    @Volatile private var running = true
    private var acceptThread: Thread? = null

    val baseUrl: String
        get() = "http://127.0.0.1:" + server.localPort

    fun on(method: String, path: String, handler: (ParityRequest, ParityResponse) -> Unit) {
        handlers["$method $path"] = handler
    }

    fun start() {
        val thread = Thread({ acceptLoop() }, "parity-fake-accept")
        thread.isDaemon = true
        acceptThread = thread
        thread.start()
    }

    fun stop() {
        running = false
        try {
            server.close()
        } catch (e: Exception) {
            // Already closed.
        }
        try {
            acceptThread?.join(500)
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
        }
    }

    fun requestCount(method: String, path: String): Int = synchronized(requests) {
        requests.count { it.method == method && it.path == path }
    }

    fun lastRequest(method: String, path: String): ParityRequest? = synchronized(requests) {
        requests.lastOrNull { it.method == method && it.path == path }
    }

    private fun acceptLoop() {
        while (running) {
            val socket = try {
                server.accept()
            } catch (e: Exception) {
                break
            }
            val thread = Thread({ handle(socket) }, "parity-fake-conn")
            thread.isDaemon = true
            thread.start()
        }
    }

    private fun handle(socket: java.net.Socket) {
        try {
            socket.use {
                val input = BufferedInputStream(socket.getInputStream())
                val requestLine = readLine(input) ?: return
                val parts = requestLine.split(' ')
                if (parts.size < 2) return
                val method = parts[0]
                val target = parts[1]
                val qIndex = target.indexOf('?')
                val path = if (qIndex < 0) target else target.substring(0, qIndex)
                val query = LinkedHashMap<String, String>()
                if (qIndex >= 0) {
                    for (pair in target.substring(qIndex + 1).split('&')) {
                        if (pair.isEmpty()) continue
                        val eq = pair.indexOf('=')
                        if (eq < 0) {
                            query[pair] = ""
                        } else {
                            query[pair.substring(0, eq)] = pair.substring(eq + 1)
                        }
                    }
                }
                val headers = LinkedHashMap<String, String>()
                while (true) {
                    val header = readLine(input) ?: return
                    if (header.isEmpty()) break
                    val colon = header.indexOf(':')
                    if (colon > 0) {
                        headers[lowerAscii(header.substring(0, colon).trim())] =
                            header.substring(colon + 1).trim()
                    }
                }
                val length = headers["content-length"]?.toIntOrNull() ?: 0
                val bodyBytes = ByteArray(length)
                var read = 0
                while (read < length) {
                    val n = input.read(bodyBytes, read, length - read)
                    if (n < 0) break
                    read += n
                }
                val request = ParityRequest(
                    method, path, query, headers,
                    String(bodyBytes, 0, read, Charsets.UTF_8)
                )
                requests.add(request)
                val response = ParityResponse(BufferedOutputStream(socket.getOutputStream()))
                val handler = handlers["$method $path"]
                if (handler == null) {
                    response.json(
                        404,
                        "{\"error\":{\"code\":\"not_found\",\"message\":\"no route\"," +
                            "\"retryable\":false}}"
                    )
                } else {
                    handler(request, response)
                }
            }
        } catch (e: Exception) {
            // Client disconnect; the assertions already recorded the request.
        }
    }

    private fun lowerAscii(text: String): String {
        val sb = StringBuilder(text.length)
        for (c in text) {
            sb.append(if (c in 'A'..'Z') (c + 32).toChar() else c)
        }
        return sb.toString()
    }

    private fun readLine(input: InputStream): String? {
        val out = StringBuilder()
        while (true) {
            val c = input.read()
            if (c < 0) return if (out.isEmpty()) null else out.toString()
            if (c == '\n'.toInt()) return out.toString()
            if (c != '\r'.toInt()) out.append(c.toChar())
            if (out.length > 65536) return out.toString()
        }
    }
}
