// JetBrains parity smoke (no display, no JUnit): canned native frames drive
// EVERY parity surface through the real panel code, a raw-socket fake
// daemon drives the real FaktorFrontendService + FaktorChatPanel end to
// end, and the real daemon is used where only it can answer (restart /
// terminal spawn). Families:
//
//   1. Faktor-owned tree: the repository root resolves and no vendored
//      upstream UI corpus exists (offline, no network)
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
import java.util.Collections
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import javax.swing.SwingUtilities

object JetBrainsParitySmoke {

    private var failures = 0

    /** Set when the executable parity matrix ran inside the fake-daemon session. */
    private var matrixHit = false

    @JvmStatic
    fun main(args: Array<String>) {
        step("tree: Faktor-owned repository root, no vendored upstream UI corpus") {
            verifyFaktorOwnedTree()
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
        step("task start single-flight: queued double click enqueues ONE request") {
            taskStartDoubleClickStep()
        }
        step("task start single-flight: Enter + click race enqueues one start") {
            taskStartEnterClickStep()
        }
        step("task start single-flight: three rapid submits enqueue one start") {
            taskStartBurstStep()
        }
        step("task start submission id: lost response retry reuses the id, receipt and no second prompt") {
            taskStartRetryStep()
        }
        step("task start single-flight: success re-enables and the next start gets a new id") {
            taskStartSuccessStep()
        }
        step("task start single-flight: typed refusal re-enables, keeps the draft and starts fresh next") {
            taskStartRefusalStep()
        }
        step("task start single-flight: transport failure re-enables and keeps draft + attachments") {
            taskStartTransportPreservationStep()
        }
        step("task start immutability: edits while pending never reach the in-flight request") {
            taskStartImmutabilityStep()
        }
        step("agent-control dialogs: input on the EDT, control on the worker, cancel is a no-op") {
            agentControlDialogsStep()
        }

        step("prompt submission identity: identical retry reuses the id, edits mint new") {
            val first = selectPromptSubmission(null, "7", "ship it") { "id-1" }
            assertEquals("id-1", first.submissionId)
            val retried = selectPromptSubmission(first, "7", "ship it") { "id-2" }
            assertEquals(
                "id-1",
                retried.submissionId,
                "an identical retry must reuse the submission id"
            )
            val edited = selectPromptSubmission(first, "7", "ship it now") { "id-2" }
            assertEquals("id-2", edited.submissionId, "a changed draft mints a new id")
            val otherSession = selectPromptSubmission(first, "8", "ship it") { "id-3" }
            assertEquals("id-3", otherSession.submissionId, "a changed session mints a new id")
        }
        step("real daemon: history/task/permissions/terminal/restart/reconnect") {
            realDaemonSuite(args[0])
        }

        // The executable parity matrices: every row runs against the canned
        // frames AND the fake daemon, the visual matrix renders each panel
        // against the pinned baselines, and the artifact is written to
        // target/certification/jetbrains-parity.json bound to this HEAD.
        step("visual certification policy: platform records are never inherited") {
            ParityMatrix.visualCertificationPolicySelfTest()
        }
        step("parity matrix: behavioral rows + visual render vs pinned baselines + artifact") {
            assertTrue(
                matrixHit,
                "the parity matrix did not run inside the fake-daemon session"
            )
            assertEquals(
                0, ParityMatrix.behavioralFailures(),
                "parity matrix behavioral rows failed (see the artifact row evidence)"
            )
            for (result in ParityMatrix.visuals()) {
                // Audit 28: drift against THIS platform's own record is a
                // hard failure; a platform with no baseline record is
                // surfaced as not certified (never compared against another
                // platform's or a canonical digest).
                assertTrue(
                    !result.drifted,
                    "visual row ${result.panel}: ${result.detail}"
                )
                if (result.notCertified) {
                    println(
                        "VISUAL NOT CERTIFIED ON PLATFORM ${result.platform}: " +
                            "${result.panel}: ${result.detail}"
                    )
                }
            }
            println(
                "VISUAL PLATFORM COVERAGE: " +
                    ParityMatrix.visualPlatformCoverage().entries.joinToString(", ") {
                        "${it.key}=${it.value}"
                    }
            )
        }

        println(if (failures == 0) "JETBRAINS PARITY SMOKE PASS" else "JETBRAINS PARITY SMOKE FAIL ($failures)")
        kotlin.system.exitProcess(if (failures == 0) 0 else 1)
    }

    // ------------------------------------------------- 1. Faktor-owned tree

    /**
     * The Faktor-owned tree contract (offline, no network): the repository
     * root resolves, the retired vendored UI corpora are GONE, and the
     * Faktor-owned UI surfaces exist. A reappearing vendored upstream tree
     * (with or without a manifest) is a hard failure.
     */
    private fun verifyFaktorOwnedTree() {
        val root = resolveRepoRoot()
        // ui/ carries ONLY the retained historical attribution directory: any
        // vendored source tree or pin manifest reappearing there fails.
        val uiEntries = File(root, "ui").listFiles()?.map { it.name }?.sorted() ?: emptyList()
        assertEquals(
            listOf("LICENSES"), uiEntries,
            "ui/ must carry only the historical attribution directory"
        )
        assertTrue(!File(root, "compat").exists(), "no pinned compatibility corpus may exist")
        assertTrue(
            !File(root, "ui/upstream.json").exists(),
            "no vendored-UI pin manifest may exist"
        )
        for (owned in listOf(
            "apps/vscode/media/chat.js",
            "apps/vscode/src/webview.ts",
            "apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt"
        )) {
            assertTrue(File(root, owned).isFile, "Faktor-owned UI surface must exist: $owned")
        }
        println("FAKTOR-OWNED TREE PASS: no vendored upstream UI corpus, panel sources present")
    }

    internal fun resolveRepoRoot(): File {
        val prop = System.getProperty("faktor.repo.root")
        if (prop != null && prop.isNotEmpty()) {
            val root = File(prop).absoluteFile
            assertTrue(isRepoRoot(root), "faktor.repo.root is not a Faktor repository root: $root")
            return root
        }
        var dir: File? = File(".").absoluteFile
        var guard = 0
        while (dir != null && guard < 8) {
            if (isRepoRoot(dir)) return dir
            dir = dir.parentFile
            guard++
        }
        fail("cannot locate the repository root (Cargo.toml + crates/); pass -Dfaktor.repo.root")
    }

    private fun isRepoRoot(dir: File): Boolean =
        File(dir, "Cargo.toml").isFile && File(dir, "crates").isDirectory

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
        val agents = parseNativeAgents(PARITY_AGENTS_JSON)
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
        val permissions = parseNativePermissionList(PARITY_PERMISSION_LIST_JSON)
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
        // The reply body is the strict session-scoped daemon DTO; the entry
        // owns session 9, which is what the reply must name.
        assertEquals("9", permission.sessionId)
        assertEquals(
            "{\"session_id\":\"9\",\"permission_id\":\"7\",\"decision\":\"allow\"}",
            NativeRequests.permissionReply(permission.sessionId, permission.id, "allow")
        )
        // A typed 409 refusal is recorded as an explicit panel state, and a
        // later clean view clears it.
        panel.setReplyRefusal("permission 7 is unknown, expired or already resolved (409 conflict)")
        assertTrue(
            panel.refusalText()!!.contains("409 conflict"),
            panel.refusalText() ?: "no refusal"
        )
        assertTrue(panel.headerText().contains("last reply refused"), panel.headerText())
        panel.update(permissions)
        assertEquals(null, panel.refusalText())
        assertEquals(false, panel.headerText().contains("refused"), panel.headerText())
    }

    // ----------------------------------------------------------- 5. terminal

    private fun terminalCanned() {
        val page = parseNativeTerminalPage(PARITY_TERMINALS_JSON)
        assertEquals("7", page.sessionId)
        assertEquals(1, page.terminals.size)
        assertEquals("5", page.terminals[0].id)
        assertEquals(true, page.terminals[0].alive)
        assertEquals("3", page.terminals[0].taskId)
        assertEquals(null, page.terminals[0].agentId)
        assertEquals(1L, page.unowned)
        val events = parseNativeTerminalEventPage(PARITY_TERMINAL_EVENTS_JSON)
        assertEquals(1, events.events.size)
        assertEquals("created", events.events[0].type)
        assertEquals(false, events.hasMore)
        assertEquals("6", parseNativeTerminalSpawned(PARITY_TERMINAL_SPAWNED_JSON).ptyId)
        val output = parseNativeTerminalOutput("6", PARITY_TERMINAL_OUTPUT_JSON)
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
        panel.setComposerFields("bash", "-lc\necho-parity", "/tmp")
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
        val view = TaskTree.tournamentView(parseNativeTournament(PARITY_TOURNAMENT_JSON))
        val panel = TournamentPanel()
        panel.setSummaries(dev.faktor.shared.parseNativeTournamentSummaries(PARITY_TOURNAMENTS_LIST_JSON))
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
        val providers = parseNativeProviders(PARITY_PROVIDERS_JSON)
        assertEquals(2, providers.size)
        assertEquals("alpha", providers[0].instanceId)
        assertEquals("openai", providers[0].family)
        assertEquals(1, providers[0].models.size)
        assertEquals(true, providers[0].models[0].reasoning)
        assertEquals(true, providers[0].runtimeContextLimitSupported)
        val panel = SettingsPanel()
        panel.setDaemonInfo("/bin/faktor-cli", "/tmp/data", "1.2.3", "http://127.0.0.1:9")
        assertTrue(panel.daemonText().contains("/bin/faktor-cli"), panel.daemonText())
        assertEquals(listOf("default", "shadow"), panel.mutationModes())
        assertEquals(null, panel.mutationMode())
        // The removed direct-owner mode is not offered and cannot be
        // selected: the surface stays shadow-only.
        panel.selectMutationMode("direct_compat")
        assertEquals(null, panel.mutationMode())
        panel.selectMutationMode("shadow")
        assertEquals("shadow", panel.mutationMode())
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
        val catalog = parseNativeModelCatalog(PARITY_DUAL_MODELS_JSON)
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
        val child = parseNativeAgents(PARITY_AGENTS_JSON)[1]
        val model = TaskTree.build(
            agents = listOf(child.copy(provider = "alpha"), child.copy(agentId = "child-b", provider = "beta")),
            catalog = catalog
        )
        assertEquals(true, model.children[0].reasoning)
        assertEquals(false, model.children[1].reasoning)
    }

    // ---------------------------------------------------------- 10. history

    private fun historyCanned() {
        val sessions = parseNativeSessionList(PARITY_SESSIONS_JSON)
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
    //
    // The fake-daemon half of the parity matrix: one named observable per
    // surface, driven in a single end-to-end session against the raw-socket
    // fake daemon. The observables assert the EXACT request bodies and the
    // rendered panel state; a failure is recorded against its surface row in
    // `target/certification/jetbrains-parity.json`.

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
        // The session workspace root: attachment picks are relativized
        // against it and session creation carries it, so workspace source
        // files reach the `files` array as relative paths.
        val workspace = Files.createTempDirectory("faktor-parity-fake-workspace-")
        var panel: FaktorChatPanel? = null
        val registry = ParityMatrixRegistry
        // The parity matrix owns `current` for this run; only the observable
        // registry is ours to clear between suites.
        registry.observables.clear()
        try {
            service.attachConnection(connection, stopAction = { process.destroyForcibly() })
            val created = service.createSession("alpha", "m", workspace.toString(), title = "parity")
            assertEquals("7", created.id)
            service.watchSession("7", 0)
            panel = FaktorChatPanel(service, workspaceRoot = workspace)
            assertTrue(panel.refreshNowForTest(), "one refresh cycle must finish")
            val chat = panel

            registry.observables["history"] = {
                // Durable session listing with the current session marked.
                assertEquals(2, chat.historyView().count())
                assertTrue(
                    chat.historyView().label(0).contains("[ready]"),
                    chat.historyView().label(0)
                )
                assertTrue(
                    chat.historyView().streamText().contains("session=7"),
                    chat.historyView().streamText()
                )
            }
            registry.observables["provider_selection"] = {
                // The registry view drives both combos and the join is
                // (provider, model).
                assertEquals(2, chat.settingsView().providerCount())
                assertEquals("alpha", chat.settingsView().selectedProvider())
                assertEquals("m", chat.settingsView().selectedModel())
            }
            registry.observables["permissions"] = {
                // Pending list rendered; the reply posts the strict DTO with
                // the OWNING session id (the entry belongs to session 9).
                // Await the rendered list first: the fake daemon's list
                // response races the panel's first paint under load.
                await("permission list rendered") { chat.permissionsView().count() == 1 }
                assertEquals(1, chat.permissionsView().count())
                assertTrue(
                    chat.permissionsView().unitLabel(0).contains("capability=shell"),
                    chat.permissionsView().unitLabel(0)
                )
                chat.permissionsView().submitReply("deny")
                await("permission reply routed") {
                    daemon.requestCount("POST", "/native/permission/reply") >= 1
                }
                assertEquals(
                    "{\"session_id\":\"9\",\"permission_id\":\"7\",\"decision\":\"deny\"}",
                    daemon.lastRequest("POST", "/native/permission/reply")!!.body
                )
                // Typed 409 conflict (unknown/expired/already resolved) is
                // surfaced as an explicit panel state (naming the exact id),
                // never retried.
                chat.permissionsView().submitReply("allow")
                await("typed conflict surfaced") {
                    chat.permissionsView().refusalText()?.contains("409 conflict") == true
                }
                assertTrue(
                    chat.permissionsView().refusalText()!!.contains("permission 7"),
                    chat.permissionsView().refusalText() ?: "no refusal"
                )
                assertEquals(
                    "{\"session_id\":\"9\",\"permission_id\":\"7\",\"decision\":\"allow\"}",
                    daemon.lastRequest("POST", "/native/permission/reply")!!.body
                )
                assertEquals(
                    2,
                    daemon.requestCount("POST", "/native/permission/reply"),
                    "a typed 409 conflict is never retried"
                )
                // Typed 409 permission_session_mismatch is surfaced too.
                chat.permissionsView().submitReply("deny")
                await("typed session mismatch surfaced") {
                    chat.permissionsView().refusalText()
                        ?.contains("permission_session_mismatch") == true
                }
                assertTrue(
                    chat.permissionsView().headerText().contains("last reply refused"),
                    chat.permissionsView().headerText()
                )
                assertEquals(
                    3,
                    daemon.requestCount("POST", "/native/permission/reply"),
                    "a session mismatch is never retried"
                )
            }
            registry.observables["terminal"] = {
                // Session-owned rows, spawn body, output snapshot.
                assertEquals(1, chat.terminalView().terminalCount())
                assertTrue(
                    chat.terminalView().terminalLabel(0).contains("task=3"),
                    chat.terminalView().terminalLabel(0)
                )
                chat.terminalView().setComposerFields("bash", "-lc\necho-parity", "/tmp")
                chat.terminalView().submitSpawn()
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
            }
            registry.observables["agents"] = {
                // Children + blockers rendered from the fake frames.
                val tree = chat.taskTreeView().model() ?: fail("the tree must be populated")
                assertEquals(1, tree.children.size)
                assertEquals("child-1", tree.children[0].childId)
                assertEquals("permission", tree.children[0].blocker?.kind)
                assertEquals(1, chat.blockersView().blockerCount())
            }
            registry.observables["task_runs"] = {
                // One request carries criteria + mutation mode + completion
                // contract + the attachment set; the served task view then
                // carries acceptance criteria.
                chat.settingsView().selectMutationMode("shadow")
                chat.completionCommit.isSelected = true
                // Audit 13: an ordinary workspace SOURCE file stays on the
                // workspace-relative `files` vocabulary and is never uploaded
                // (no blind uploads): the absolute pick is relativized
                // against the session workspace root.
                val source = Files.createTempFile(workspace, "faktor-parity-source-", ".rs")
                Files.write(source, "fn main() {}".toByteArray())
                chat.attachmentsView().addFiles(listOf(source.toString()))
                chat.setTaskFieldsForTest("parity goal", "criterion A, criterion B")
                chat.submitTaskForTest()
                await("task run routed") {
                    daemon.lastRequest("POST", "/native/session/7/task-runs") != null
                }
                val taskBody = daemon.lastRequest("POST", "/native/session/7/task-runs")!!.body
                assertTrue(taskBody.contains("\"goal\":\"parity goal\""), taskBody)
                assertTrue(
                    taskBody.contains("\"criteria\":[\"criterion A\",\"criterion B\"]"), taskBody
                )
                assertTrue(taskBody.contains("\"mutation_mode\":\"shadow\""), taskBody)
                assertTrue(taskBody.contains("\"completion_contract\":{\"include_commit\":true"), taskBody)
                assertTrue(
                    taskBody.contains(
                        "\"files\":[\"" + source.fileName.toString() + "\"]"
                    ),
                    "the source file must ride the WORKSPACE-RELATIVE `files` array: $taskBody"
                )
                assertTrue(
                    !taskBody.contains(source.toString()),
                    "an absolute pick must never ride the request: $taskBody"
                )
                assertEquals(
                    0,
                    daemon.requestCount("POST", "/native/session/7/attachments"),
                    "a workspace source file is never uploaded"
                )
                chat.attachmentsView().clear()
                await("task refresh after start") {
                    chat.taskTreeView().model()?.steps?.isNotEmpty() ?: false
                }

                // Audit 13: the advertised document contract accepts
                // application/pdf + text/plain as durable attachments; the
                // run carries their typed ids, never the filesystem paths.
                val pdf = Files.createTempFile(workspace, "faktor-parity-doc-", ".pdf")
                Files.write(pdf, "%PDF-1.4 parity".toByteArray())
                val txt = Files.createTempFile(workspace, "faktor-parity-doc-", ".txt")
                Files.write(txt, "plain parity".toByteArray())
                chat.attachmentsView().addFiles(listOf(pdf.toString(), txt.toString()))
                val runsBeforeDocs = daemon.requestCount("POST", "/native/session/7/task-runs")
                chat.submitTaskForTest()
                await("document uploads routed") {
                    daemon.requestCount("POST", "/native/session/7/attachments") >= 2 &&
                        daemon.requestCount("POST", "/native/session/7/task-runs") > runsBeforeDocs
                }
                val documentBodies = attachmentBodies(daemon)
                assertTrue(
                    documentBodies.any { it.contains("\"mime\":\"application/pdf\"") },
                    "the PDF must upload as an application/pdf attachment: $documentBodies"
                )
                assertTrue(
                    documentBodies.any { it.contains("\"mime\":\"text/plain\"") },
                    "the text document must upload as a text/plain attachment: $documentBodies"
                )
                val documentRun = daemon.lastRequest("POST", "/native/session/7/task-runs")!!.body
                val documentMimes = attachmentMimes(documentRun)
                assertEquals(
                    listOf("application/pdf", "text/plain"), documentMimes,
                    "the run carries the durable document ids in entry order: $documentRun"
                )
                assertTrue(
                    !documentRun.contains(pdf.toString()) && !documentRun.contains(txt.toString()),
                    "delivered documents must not also ride workspace paths: $documentRun"
                )
                chat.attachmentsView().clear()

                // Audit 12: a clipboard BufferedImage converts to bounded PNG
                // bytes IN MEMORY (no file, no path) and reaches a durable
                // NativeAttachmentId through the SAME upload path.
                val clipboardImage = java.awt.image.BufferedImage(
                    2, 2, java.awt.image.BufferedImage.TYPE_INT_ARGB
                )
                clipboardImage.setRGB(0, 0, 0xFF00FF00.toInt())
                clipboardImage.setRGB(1, 1, 0xFF0000FF.toInt())
                assertTrue(
                    chat.attachmentsView().addClipboardImage(clipboardImage, "clipboard.png"),
                    "the in-memory clipboard image must stage as a pending binary"
                )
                assertEquals(1, chat.attachmentsView().binaryCount())
                val runsBeforeClipboard = daemon.requestCount("POST", "/native/session/7/task-runs")
                chat.submitTaskForTest()
                await("clipboard PNG upload routed") {
                    attachmentBodies(daemon).any { it.contains("\"mime\":\"image/png\"") } &&
                        daemon.requestCount("POST", "/native/session/7/task-runs") > runsBeforeClipboard
                }
                val pngBody = attachmentBodies(daemon).last { it.contains("\"mime\":\"image/png\"") }
                assertTrue(
                    pngBody.contains("\"filename\":\"clipboard.png\""),
                    "the clipboard attachment is named, not a filesystem path: $pngBody"
                )
                val pngBytes = java.util.Base64.getDecoder().decode(
                    JsonCodec.parse(pngBody).view("clipboard upload").field("data_base64").string()
                )
                assertEquals(0x89.toByte(), pngBytes[0], "the uploaded bytes are a PNG")
                assertEquals('N'.toInt().toByte(), pngBytes[2], "the uploaded bytes are a PNG")
                val clipboardRun = daemon.lastRequest("POST", "/native/session/7/task-runs")!!.body
                assertEquals(
                    listOf("image/png"), attachmentMimes(clipboardRun),
                    "the run carries the clipboard image's durable id: $clipboardRun"
                )
                chat.attachmentsView().clear()

                // Typed refusal: an over-bound document is refused BEFORE any
                // upload and surfaces its exact advertised bound.
                val oversize = Files.createTempFile(workspace, "faktor-parity-big-", ".pdf")
                Files.write(oversize, ByteArray(5000) { 1 })
                chat.attachmentsView().addFiles(listOf(oversize.toString()))
                val uploadsBeforeRefusal = attachmentBodies(daemon).size
                val runsBeforeRefusal = daemon.requestCount("POST", "/native/session/7/task-runs")
                chat.submitTaskForTest()
                await("typed oversized document refusal surfaces") {
                    chat.transcriptTextForTest().contains("per-document bound")
                }
                assertEquals(
                    uploadsBeforeRefusal,
                    attachmentBodies(daemon).size,
                    "a refused document uploads nothing"
                )
                assertEquals(
                    runsBeforeRefusal,
                    daemon.requestCount("POST", "/native/session/7/task-runs"),
                    "a refused document never starts a run"
                )
                chat.attachmentsView().clear()
            }
            registry.observables["criterion_proofs"] = {
                // The verification route's records render every binding kind
                // with its exact reference (daemon-served, never guessed).
                // The served task view carries no explicit acceptance list, so
                // rows are record-backed: seven typed kinds + the honest
                // unavailable row; the canned half owns the exact count.
                val model = chat.taskTreeView().model() ?: fail("the tree must be populated")
                assertEquals(PARITY_BINDING_KINDS.size + 1, model.criteriaProof.size)
                for ((criterion, kind, reference) in PARITY_BINDING_KINDS) {
                    val row = model.criteriaProof.firstOrNull { it.criterionKey == criterion }
                        ?: fail("missing proof row $criterion")
                    assertEquals(kind, row.bindingKind, criterion)
                    assertEquals("daemon", row.bindingSource, criterion)
                    assertEquals(reference, row.bindingReference, criterion)
                }
            }
            registry.observables["evidence"] = {
                // Evidence refs from the served verification render on the
                // navigator; retrieval routes through the real native route.
                val model = chat.taskTreeView().model() ?: fail("the tree must be populated")
                assertTrue(
                    model.evidence.isNotEmpty(),
                    "the served verification must carry evidence refs"
                )
                val retrieval = service.retrieveEvidence(41L, "{\"selector\":\"all\"}")
                assertEquals(41L, retrieval.id)
                val text = String(retrieval.bytes, Charsets.UTF_8)
                assertTrue(
                    text.contains("parity tool output"),
                    "the retrieved text must carry the served payload: $text"
                )
                chat.evidenceView().setEvidence(model.evidence)
                assertTrue(
                    chat.evidenceView().evidenceCount() >= 1,
                    "the navigator renders the served evidence refs"
                )
            }
            registry.observables["tournament"] = {
                // The tournament listing route loads into the panel's
                // summaries (the daemon owns the candidates; decide gating is
                // the panel row's canned half).
                val summaries = service.tournaments()
                assertEquals(2, summaries.size)
                assertEquals("t-1", summaries[0].id)
                chat.tournamentViewForTest().setSummaries(summaries)
                assertTrue(
                    chat.tournamentViewForTest().summariesText().contains("t-1"),
                    chat.tournamentViewForTest().summariesText()
                )
            }
            registry.observables["settings"] = {
                // The shadow-only mutation vocabulary the Task composer reads:
                // the removed direct-owner mode is never offered.
                assertEquals(
                    listOf("default", "shadow"),
                    chat.settingsView().mutationModes()
                )
            }
            registry.observables["restart_reconnect"] = {
                // Opening the other durable session switches the current
                // session and reopens the SSE stream at the new cursor.
                chat.historyView().select(1)
                chat.historyView().submitOpen()
                await("session 8 opened") { service.currentSessionId() == "8" }
                assertTrue(
                    chat.historyView().reconnectEnabled(),
                    "reconnect stays available on the durable session surface"
                )
            }
            // The executable parity matrix runs NOW, inside the live
            // fake-daemon session: its daemon rows drive the real clients
            // against this daemon, its visual rows render the panels and the
            // artifact lands on disk before the session is torn down.
            ParityMatrix.run()
            matrixHit = true
        } finally {
            if (panel != null) panel.shutdown()
            service.stop()
            daemon.stop()
            registry.observables.clear()
            workspace.toFile().deleteRecursively()
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
                .filter { it.path == "/native/session/7/events" && it.query["after"] == "2" }
            assertTrue(
                resumedRequests.isNotEmpty(),
                "the second SSE connection must resume at after=2"
            )
        } finally {
            service.stop()
            daemon.stop()
        }
    }

    // ------------------------------------- fake daemon: task-start single flight

    /**
     * One harness per scenario: a fresh fake daemon, a real
     * FaktorFrontendService attached to it, and the real panel. The task-run
     * POST route is the observable under test and can be overridden per
     * scenario; every other route is the shared parity fixture.
     */
    private fun startTaskHarness(
        runs: ((ParityRequest, ParityResponse) -> Unit)? = null
    ): TaskStartHarness {
        val daemon = ParityFakeDaemon()
        registerRoutes(daemon)
        if (runs != null) daemon.on("POST", "/native/session/7/task-runs", runs)
        daemon.start()
        val process = ProcessBuilder(javaBinary(), "-version").start()
        val connection = BackendConnection(
            daemon.server.localPort, PARITY_PASSWORD, process, StdoutSink(process)
        )
        val service = FaktorFrontendService(
            Paths.get("unused"),
            Paths.get(System.getProperty("java.io.tmpdir"), "faktor-parity-single-flight")
        )
        // The session workspace root: the panel relativizes attachment picks
        // against it and the daemon refuses absolute `files`, so session
        // creation carries the same root.
        val workspace = Files.createTempDirectory("faktor-parity-single-flight-ws-")
        service.attachConnection(connection, stopAction = { process.destroyForcibly() })
        service.createSession("alpha", "m", workspace.toString(), title = "single flight")
        service.watchSession("7", 0)
        return TaskStartHarness(
            daemon, service, FaktorChatPanel(service, workspaceRoot = workspace), process, workspace
        )
    }

    private fun taskStartBodies(daemon: ParityFakeDaemon): List<String> =
        synchronized(daemon.requests) {
            daemon.requests
                .filter { it.method == "POST" && it.path == "/native/session/7/task-runs" }
                .map { it.body }
        }

    private fun submissionIdOf(body: String): String =
        JsonCodec.parse(body).view("task start").field("submission_id").string()

    private fun countOccurrences(text: String, needle: String): Int {
        var count = 0
        var index = text.indexOf(needle)
        while (index >= 0) {
            count += 1
            index = text.indexOf(needle, index + needle.length)
        }
        return count
    }

    private fun taskStartDoubleClickStep() {
        val harness = startTaskHarness()
        try {
            harness.panel.setTaskFieldsForTest("double click goal", "")
            harness.panel.submitTaskForTest()
            harness.panel.submitTaskForTest()
            await("one start request after a queued double click") {
                harness.daemon.requestCount("POST", "/native/session/7/task-runs") == 1
            }
            await("the single start completes") {
                harness.panel.transcriptTextForTest().contains("run-9 started")
            }
            assertEquals(
                1,
                harness.daemon.requestCount("POST", "/native/session/7/task-runs"),
                "a queued double click must enqueue exactly ONE start request"
            )
        } finally {
            harness.close()
        }
    }

    private fun taskStartEnterClickStep() {
        val harness = startTaskHarness()
        try {
            harness.panel.setTaskFieldsForTest("enter race goal", "")
            harness.panel.submitTaskViaEnterForTest()
            harness.panel.submitTaskForTest()
            await("one start request after the Enter + click race") {
                harness.daemon.requestCount("POST", "/native/session/7/task-runs") == 1
            }
            await("the single start completes") {
                harness.panel.transcriptTextForTest().contains("run-9 started")
            }
            assertEquals(
                1,
                harness.daemon.requestCount("POST", "/native/session/7/task-runs"),
                "Enter + click must enqueue exactly ONE start request"
            )
        } finally {
            harness.close()
        }
    }

    private fun taskStartBurstStep() {
        val harness = startTaskHarness()
        try {
            harness.panel.setTaskFieldsForTest("burst goal", "")
            harness.panel.submitTaskForTest()
            harness.panel.submitTaskForTest()
            harness.panel.submitTaskForTest()
            await("the single start completes") {
                harness.panel.transcriptTextForTest().contains("run-9 started")
            }
            assertEquals(
                1,
                harness.daemon.requestCount("POST", "/native/session/7/task-runs"),
                "three rapid submits must enqueue exactly ONE start request"
            )
        } finally {
            harness.close()
        }
    }

    private fun taskStartRetryStep() {
        val attempts = AtomicInteger()
        val harness = startTaskHarness { _, response ->
            // The first attempt closes without a response: a lost HTTP response.
            if (attempts.incrementAndGet() > 1) response.json(200, TASK_RUN_STARTED_JSON)
        }
        try {
            harness.panel.setTaskFieldsForTest("retry goal", "")
            harness.panel.submitTaskForTest()
            await("first attempt recorded and transport failure surfaced") {
                attempts.get() == 1 &&
                    harness.panel.transcriptTextForTest().contains("transport failure")
            }
            await("controls re-enabled after the transport failure") {
                harness.panel.startTaskEnabledForTest()
            }
            harness.panel.submitTaskForTest()
            await("the same-id retry is accepted") {
                harness.panel.transcriptTextForTest().contains("run-9 started")
            }
            val bodies = taskStartBodies(harness.daemon)
            assertEquals(2, bodies.size, "a lost response retry is ONE extra immutable submission")
            assertEquals(
                submissionIdOf(bodies[0]),
                submissionIdOf(bodies[1]),
                "the retry of the same immutable submission must reuse its id: $bodies"
            )
            assertTrue(
                harness.daemon.requests.none { it.path.contains("prompt") },
                "a task-start retry must not re-prompt"
            )
        } finally {
            harness.close()
        }
    }

    private fun taskStartSuccessStep() {
        val harness = startTaskHarness()
        try {
            val panel = harness.panel
            panel.setTaskFieldsForTest("success goal", "")
            panel.submitTaskForTest()
            await("first start completes") {
                panel.transcriptTextForTest().contains("run-9 started")
            }
            assertTrue(panel.startTaskEnabledForTest(), "the start control re-enables on success")
            assertTrue(panel.newSessionEnabledForTest(), "the new-task action re-enables on success")
            panel.submitTaskForTest()
            await("second start completes") {
                harness.daemon.requestCount("POST", "/native/session/7/task-runs") == 2 &&
                    countOccurrences(panel.transcriptTextForTest(), "run-9 started") == 2
            }
            val bodies = taskStartBodies(harness.daemon)
            assertTrue(
                submissionIdOf(bodies[0]) != submissionIdOf(bodies[1]),
                "success clears the submission, so the next start gets a NEW id: $bodies"
            )
        } finally {
            harness.close()
        }
    }

    private fun taskStartRefusalStep() {
        val attempts = AtomicInteger()
        val harness = startTaskHarness { _, response ->
            if (attempts.incrementAndGet() == 1) {
                response.json(409, TASK_START_CONFLICT_JSON)
            } else {
                response.json(200, TASK_RUN_STARTED_JSON)
            }
        }
        try {
            val panel = harness.panel
            val source = Files.createTempFile(harness.workspace, "faktor-single-flight-refusal-", ".rs")
            Files.write(source, "fn main() {}".toByteArray())
            panel.attachmentsView().addFiles(listOf(source.toString()))
            panel.setTaskFieldsForTest("refusal goal", "a, b")
            panel.submitTaskForTest()
            await("typed refusal surfaced") {
                panel.transcriptTextForTest().contains("task start refused: 409 conflict")
            }
            assertTrue(panel.startTaskEnabledForTest(), "the start control re-enables on refusal")
            assertEquals("refusal goal", panel.goalTextForTest(), "a typed refusal keeps the draft")
            assertEquals(1, panel.attachmentsCountForTest(), "a typed refusal keeps attachments")
            panel.submitTaskForTest()
            await("the next logical start completes") {
                harness.daemon.requestCount("POST", "/native/session/7/task-runs") == 2
            }
            val bodies = taskStartBodies(harness.daemon)
            assertTrue(
                submissionIdOf(bodies[0]) != submissionIdOf(bodies[1]),
                "a typed refusal clears the submission, so the next start gets a NEW id: $bodies"
            )
        } finally {
            harness.close()
        }
    }

    private fun taskStartTransportPreservationStep() {
        val attempts = AtomicInteger()
        val harness = startTaskHarness { _, response ->
            if (attempts.incrementAndGet() > 1) response.json(200, TASK_RUN_STARTED_JSON)
        }
        try {
            val panel = harness.panel
            val source = Files.createTempFile(harness.workspace, "faktor-single-flight-transport-", ".rs")
            Files.write(source, "fn main() {}".toByteArray())
            panel.attachmentsView().addFiles(listOf(source.toString()))
            panel.setTaskFieldsForTest("transport goal", "c1")
            panel.submitTaskForTest()
            await("transport failure surfaced") {
                panel.transcriptTextForTest().contains("transport failure")
            }
            assertTrue(panel.startTaskEnabledForTest(), "the start control re-enables on failure")
            assertEquals("transport goal", panel.goalTextForTest(), "the draft survives the failure")
            assertEquals(1, panel.attachmentsCountForTest(), "attachments survive the failure")
            panel.submitTaskForTest()
            await("the retained submission is accepted") {
                panel.transcriptTextForTest().contains("run-9 started")
            }
            val bodies = taskStartBodies(harness.daemon)
            assertEquals(2, bodies.size, "exactly one retry reached the daemon")
            assertEquals(
                submissionIdOf(bodies[0]),
                submissionIdOf(bodies[1]),
                "the failure retry reuses the retained submission id: $bodies"
            )
        } finally {
            harness.close()
        }
    }

    private fun taskStartImmutabilityStep() {
        val entered = CountDownLatch(1)
        val release = CountDownLatch(1)
        val harness = startTaskHarness { _, response ->
            entered.countDown()
            release.await(10, TimeUnit.SECONDS)
            response.json(200, TASK_RUN_STARTED_JSON)
        }
        try {
            val panel = harness.panel
            val captured = Files.createTempFile(harness.workspace, "faktor-single-flight-captured-", ".rs")
            Files.write(captured, "fn main() {}".toByteArray())
            panel.attachmentsView().addFiles(listOf(captured.toString()))
            panel.setTaskFieldsForTest("captured goal", "c1")
            panel.submitTaskForTest()
            assertTrue(entered.await(10, TimeUnit.SECONDS), "the start request must reach the daemon")
            val inFlight = harness.daemon.lastRequest("POST", "/native/session/7/task-runs")!!.body
            // Edits while pending: a new goal, new criteria, a new attachment
            // and a completion contract toggle must not reach the in-flight
            // request, and the submit itself must not enqueue a second start.
            panel.setTaskFieldsForTest("edited while pending", "c2")
            panel.completionCommit.isSelected = true
            val late = Files.createTempFile(harness.workspace, "faktor-single-flight-late-", ".rs")
            Files.write(late, "fn main() {}".toByteArray())
            panel.attachmentsView().addFiles(listOf(late.toString()))
            panel.submitTaskForTest()
            release.countDown()
            await("the captured submission completes") {
                panel.transcriptTextForTest().contains("run-9 started")
            }
            assertTrue(inFlight.contains("\"goal\":\"captured goal\""), inFlight)
            assertTrue(inFlight.contains("\"criteria\":[\"c1\"]"), inFlight)
            assertTrue(
                inFlight.contains("\"files\":[\"" + captured.fileName.toString() + "\"]"),
                "the captured attachment must ride the request as a workspace-relative path: $inFlight"
            )
            assertTrue(!inFlight.contains("edited while pending"), inFlight)
            assertTrue(
                !inFlight.contains(late.fileName.toString()),
                "an attachment added while pending must not reach the request: $inFlight"
            )
            assertTrue(!inFlight.contains("completion_contract"), inFlight)
            assertEquals(
                1,
                harness.daemon.requestCount("POST", "/native/session/7/task-runs"),
                "a submit while pending must not enqueue a second start"
            )
        } finally {
            release.countDown()
            harness.close()
        }
    }

    /**
     * All four agent-control dialogs (Steer / Model / Token budget / Cost
     * budget): with an injectable input provider the dialog is resolved ON
     * the EDT while the control call is dispatched on the `faktor-ui`
     * worker and reaches the daemon; a cancelled dialog calls no service.
     */
    private fun agentControlDialogsStep() {
        val harness = startTaskHarness()
        try {
            val panel = harness.panel
            assertTrue(panel.refreshNowForTest(), "one refresh cycle must populate the agents combo")
            val providerTitles = Collections.synchronizedList(ArrayList<String>())
            val providerEdt = Collections.synchronizedList(ArrayList<Boolean>())
            val dispatchThreads = Collections.synchronizedList(ArrayList<String>())
            panel.agentControlDispatchObserver = { label ->
                dispatchThreads.add(label + ":" + Thread.currentThread().name)
            }
            panel.agentInputProvider = { _, title, _ ->
                providerTitles.add(title)
                providerEdt.add(SwingUtilities.isEventDispatchThread())
                when {
                    title.startsWith("Steer") -> "steer now"
                    title.startsWith("Model") -> "m2"
                    title.contains("max_tokens") -> "4096"
                    else -> "2500000"
                }
            }

            fun selectChild() {
                assertTrue(panel.refreshNowForTest(), "a refresh cycle must finish")
                assertTrue(
                    panel.selectAgentForTest("child-1"),
                    "the agents fixture must list child-1"
                )
            }

            selectChild()
            assertTrue(panel.triggerAgentControlForTest("Steer"))
            await("steer routed") {
                harness.daemon.lastRequest("POST", "/native/agents/child-1/steer") != null
            }
            assertEquals(
                "{\"text\":\"steer now\"}",
                harness.daemon.lastRequest("POST", "/native/agents/child-1/steer")!!.body
            )

            selectChild()
            assertTrue(panel.triggerAgentControlForTest("Model"))
            await("model routed") {
                harness.daemon.lastRequest("POST", "/native/agents/child-1/model") != null
            }
            assertEquals(
                "{\"model\":\"m2\"}",
                harness.daemon.lastRequest("POST", "/native/agents/child-1/model")!!.body
            )

            selectChild()
            assertTrue(panel.triggerAgentControlForTest("Token budget"))
            await("token budget routed") {
                harness.daemon.lastRequest("POST", "/native/agents/child-1/budget") != null
            }
            assertEquals(
                "{\"max_tokens\":4096}",
                harness.daemon.lastRequest("POST", "/native/agents/child-1/budget")!!.body
            )

            selectChild()
            assertTrue(panel.triggerAgentControlForTest("Cost budget"))
            await("cost budget routed") {
                harness.daemon.lastRequest("POST", "/native/agents/child-1/budget")!!.body
                    .contains("\"max_cost_micro\"")
            }
            assertEquals(
                "{\"max_cost_micro\":2500000}",
                harness.daemon.lastRequest("POST", "/native/agents/child-1/budget")!!.body
            )

            assertEquals(4, providerTitles.size, "every dialog resolved through the provider")
            assertTrue(
                providerEdt.all { it },
                "every agent-control dialog must run ON the EDT: $providerEdt"
            )
            assertTrue(
                dispatchThreads.size == 4 && dispatchThreads.all { it.endsWith(":faktor-ui") },
                "every control call must be dispatched on the faktor-ui worker: $dispatchThreads"
            )

            // A cancelled dialog (null input) calls NO service: the agent and
            // budget POST counts stay frozen while the worker still drains
            // the job (the observer proves the dispatch happened).
            panel.agentInputProvider = { _, _, _ -> null }
            val modelCallsBefore = harness.daemon.requestCount("POST", "/native/agents/child-1/model")
            val budgetCallsBefore = harness.daemon.requestCount("POST", "/native/agents/child-1/budget")
            selectChild()
            assertTrue(panel.triggerAgentControlForTest("Model"))
            selectChild()
            assertTrue(panel.triggerAgentControlForTest("Cost budget"))
            assertEquals(
                modelCallsBefore,
                harness.daemon.requestCount("POST", "/native/agents/child-1/model"),
                "a cancelled model dialog never calls the service"
            )
            assertEquals(
                budgetCallsBefore,
                harness.daemon.requestCount("POST", "/native/agents/child-1/budget"),
                "a cancelled cost-budget dialog never calls the service"
            )
        } finally {
            harness.close()
        }
    }

    // ----------------------------------------------------- real daemon suite
    private fun realDaemonSuite(binaryPath: String) {
        val binary = Paths.get(binaryPath)
        val dataDir = Files.createTempDirectory("faktor-parity-real-")
        // The daemon's provider preflight requires a registered provider:
        // seed the discovered config so the parity rows' `default` id is
        // served (local ollama entry, no key).
        Files.write(
            dataDir.resolve("faktor-plus.json"),
            """{"config_version":1,"model":"default","providers":[{"kind":"ollama","id":"default","base_url":"http://127.0.0.1:9","allow_loopback":true}]}"""
                .toByteArray()
        )
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
                    mutationMode = "shadow",
                    submissionId = java.util.UUID.randomUUID().toString()
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
        daemon.on("POST", "/native/session") { _, response -> response.json(200, CREATED_JSON) }
        daemon.on("GET", "/native/sessions") { _, response -> response.json(200, PARITY_SESSIONS_JSON) }
        daemon.on("GET", "/models") { _, response -> response.json(200, PARITY_MODELS_JSON) }
        daemon.on("GET", "/native/providers") { _, response -> response.json(200, PARITY_PROVIDERS_JSON) }
        daemon.on("GET", "/native/usage") { _, response -> response.json(200, USAGE_JSON) }
        daemon.on("GET", "/session/7/projection") { _, response -> response.json(200, PROJECTION_JSON) }
        daemon.on("GET", "/native/messages") { _, response -> response.json(200, MESSAGES_JSON) }
        daemon.on("GET", "/native/session/7/tasks") { _, response -> response.json(200, TASK_VIEWS_JSON) }
        daemon.on("GET", "/native/session/7/task-runs") { _, response -> response.json(200, TASK_RUNS_JSON) }
        daemon.on("POST", "/native/session/7/task-runs") { _, response ->
            response.json(200, TASK_RUN_STARTED_JSON)
        }
        // Durable attachment uploads (audit 12/13): the fake echoes the
        // strict request back as a typed attachment id so the ACCEPT rows
        // prove the client consumed the advertised contract, uploaded the
        // exact bytes (document / in-memory PNG), and carried only the
        // durable ids into the task start.
        val attachmentUploads = java.util.concurrent.atomic.AtomicInteger()
        daemon.on("POST", "/native/session/7/attachments") { request, response ->
            val ordinal = attachmentUploads.incrementAndGet()
            val body = JsonCodec.parse(request.body).view("upload attachment")
            val mime = body.field("mime").string()
            val filename = body.optionalField("filename")?.string()
            val size = java.util.Base64.getDecoder()
                .decode(body.field("data_base64").string()).size
            val filenameJson =
                if (filename == null) "null" else "\"" + filename.replace("\\", "\\\\").replace("\"", "\\\"") + "\""
            response.json(
                200,
                "{\"ref_id\":" + ordinal + ",\"digest\":\"" +
                    ordinal.toString().padStart(64, '0') + "\",\"mime\":\"" +
                    mime + "\",\"filename\":" + filenameJson + ",\"size\":" + size + "}"
            )
        }
        daemon.on("GET", "/native/session/7/tasks/3/verification") { _, response ->
            response.json(200, PARITY_CRITERION_PROOF_JSON)
        }
        daemon.on("GET", "/native/session/7/verification") { _, response ->
            response.json(200, VERIFICATION_JSON)
        }
        daemon.on("POST", "/native/evidence/41/retrieve") { _, response ->
            response.json(200, PARITY_EVIDENCE_RETRIEVAL_JSON)
        }
        daemon.on("GET", "/native/session/7/usage") { _, response -> response.json(200, SESSION_USAGE_JSON) }
        daemon.on("GET", "/native/session/9/usage") { _, response -> response.json(200, SESSION_USAGE_JSON) }
        daemon.on("GET", "/native/session/7/tournaments") { _, response ->
            response.json(200, PARITY_TOURNAMENTS_LIST_JSON)
        }
        daemon.on("GET", "/native/session/7/board") { _, response -> response.json(200, PARITY_BOARD_PAGE_JSON) }
        daemon.on("GET", "/native/permissions") { _, response -> response.json(200, PARITY_PERMISSION_LIST_JSON) }
        // Reply attempts: first applies, then one typed 409 conflict and one
        // typed 409 permission_session_mismatch — the driven assertions prove
        // each refusal is surfaced and never retried.
        var permissionReplies = 0
        daemon.on("POST", "/native/permission/reply") { _, response ->
            permissionReplies += 1
            when (permissionReplies) {
                1 -> response.json(200, PERMISSION_ACK_JSON)
                2 -> response.json(409, PERMISSION_CONFLICT_JSON)
                else -> response.json(409, PERMISSION_SESSION_MISMATCH_JSON)
            }
        }
        daemon.on("GET", "/native/agents") { _, response -> response.json(200, PARITY_AGENTS_JSON) }
        // Agent-control routes: the dialog-path smoke drives all four input
        // dialogs and asserts the service call reached the daemon from the
        // worker (never from the EDT).
        daemon.on("POST", "/native/agents/child-1/steer") { _, response ->
            response.json(200, AGENT_CONTROL_ACK_JSON)
        }
        daemon.on("POST", "/native/agents/child-1/model") { _, response ->
            response.json(200, AGENT_CONTROL_ACK_JSON)
        }
        daemon.on("POST", "/native/agents/child-1/budget") { _, response ->
            response.json(200, AGENT_CONTROL_ACK_JSON)
        }
        daemon.on("GET", "/native/terminals") { _, response -> response.json(200, PARITY_TERMINALS_JSON) }
        daemon.on("GET", "/native/session/7/terminal/events") { _, response ->
            response.json(200, PARITY_TERMINAL_EVENTS_JSON)
        }
        daemon.on("POST", "/native/session/7/terminal") { _, response ->
            response.json(200, PARITY_TERMINAL_SPAWNED_JSON)
        }
        daemon.on("GET", "/native/session/7/terminals/6/output") { _, response -> response.json(200, PARITY_TERMINAL_OUTPUT_JSON) }
        daemon.on("GET", "/native/session/7/terminals/5/output") { _, response -> response.json(200, PARITY_TERMINAL_OUTPUT_JSON) }
        for (session in listOf("7", "8")) {
            daemon.on("GET", "/native/session/$session/events") { request, response ->
                val after = request.query["after"]?.toLongOrNull() ?: 0L
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

    /** Every captured attachment-upload body for session 7, in request order. */
    private fun attachmentBodies(daemon: ParityFakeDaemon): List<String> =
        synchronized(daemon.requests) {
            daemon.requests
                .filter { it.method == "POST" && it.path == "/native/session/7/attachments" }
                .map { it.body }
        }

    /** The `attachments[].mime` list of one task-run body (empty when absent). */
    private fun attachmentMimes(body: String): List<String> {
        val attachments = JsonCodec.parse(body).view("task-run").optionalField("attachments")
            ?: return emptyList()
        return attachments.array().map { it.field("mime").string() }
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

    private const val PERMISSION_ACK_JSON = "{\"ok\":true}"

    private const val PERMISSION_CONFLICT_JSON =
        "{\"error\":{\"code\":\"conflict\"," +
            "\"message\":\"permission 7 unknown or already resolved\",\"retryable\":false}}"

    private const val PERMISSION_SESSION_MISMATCH_JSON =
        "{\"error\":{\"code\":\"permission_session_mismatch\"," +
            "\"message\":\"permission 7 is owned by session 8, not session 9\"," +
            "\"retryable\":false}}"

    private const val TASK_RUN_STARTED_JSON =
        "{\"task_id\":3,\"run_id\":\"run-9\",\"state\":\"Running\"}"

    private const val TASK_START_CONFLICT_JSON =
        "{\"error\":{\"code\":\"conflict\",\"message\":\"task run already exists\"," +
            "\"retryable\":false}}"

    private const val AGENT_CONTROL_ACK_JSON = "{}"

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

/** One task-start scenario: the fake daemon, the attached service and the real panel. */
private class TaskStartHarness(
    val daemon: ParityFakeDaemon,
    val service: FaktorFrontendService,
    val panel: FaktorChatPanel,
    private val process: Process,
    val workspace: java.nio.file.Path
) {
    fun close() {
        panel.shutdown()
        service.stop()
        daemon.stop()
        process.destroyForcibly()
        workspace.toFile().deleteRecursively()
    }
}
