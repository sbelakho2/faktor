// JetBrains BEHAVIORAL + VISUAL parity matrices (executable artifacts).
//
// This file is the parity-matrix engine behind `JetBrainsParitySmoke`: every
// parity surface is a ROW with a name, a canned-native-frames check and a
// fake-daemon check, each recording pass/fail plus the EXACT observable it
// checked. The engine also renders every Faktor Swing panel into an offscreen
// `BufferedImage` and compares a canonical component-tree/state digest against
// pinned baselines (the visual matrix), then writes
//
//   target/certification/jetbrains-parity.json
//
// `{schema, commit, rows[], behavioral{passed,total}, visual{passed,total,
//  method}}`, bound to the exact HEAD. `scripts/capabilities-manifest.mjs`
// consumes that artifact and never a smoke suite's green output.
//
// The pinned visual baselines live in
// `apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json`
// and are written ONLY when `-Dfaktor.parity.writeBaselines=true` is passed
// (the smoke script's `--write-baselines` flag): a normal run compares
// against the pinned file and reports drift as a failure.
package dev.faktor.frontend

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
import dev.faktor.shared.parseNativeTaskVerification
import dev.faktor.shared.parseNativeTaskViews
import dev.faktor.shared.parseNativeTerminalEventPage
import dev.faktor.shared.parseNativeTerminalOutput
import dev.faktor.shared.parseNativeTerminalPage
import dev.faktor.shared.parseNativeTerminalSpawned
import dev.faktor.shared.parseNativeTournament
import dev.faktor.shared.parseNativeTournamentSummaries
import dev.faktor.shared.view
import java.awt.Component
import java.awt.Container
import java.awt.image.BufferedImage
import java.io.File
import java.security.MessageDigest
import java.util.LinkedHashMap

/** One parity row: identity + the observables its two checks record. */
internal data class ParityRow(
    val surface: String,
    val canned: () -> Unit,
    val daemonCheck: () -> Unit
)

internal data class ParityFailure(val surface: String, val check: String, val message: String)

internal data class ParityVisualResult(
    val panel: String,
    val passed: Boolean,
    val digest: String,
    val baseline: String?,
    val detail: String
)

private val PARITY_PATH = "target/certification/jetbrains-parity.json"
private val PARITY_BASELINE_PATH =
    "apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json"

/** The matrix engine: run every row against both sources, then snapshot. */
internal object ParityMatrix {

    private var registered = false
    private var ranLocally = false
    private var daemonDriven = false
    private var finished = false
    private val rows = ArrayList<ParityRow>()
    private val cannedFailures = ArrayList<ParityFailure>()
    private val daemonFailures = ArrayList<ParityFailure>()
    private var visualResults: List<ParityVisualResult> = emptyList()

    private fun register(row: ParityRow) {
        rows.add(row)
    }

    // ------------------------------------------------------------- canned

    /**
     * Runs every row's canned check and records the exact failing observable.
     * A canned check only ever throws on a real mismatch; the failure text is
     * carried into the artifact row evidence.
     */
    fun runCanned() {
        for (row in rows) {
            try {
                row.canned()
            } catch (t: Throwable) {
                cannedFailures.add(
                    ParityFailure(row.surface, "canned", t.message ?: t.toString())
                )
            }
        }
    }

    // ------------------------------------------------------------- daemon

    fun runDaemon() {
        if (daemonDriven) {
            // Already driven inside the fake-daemon suite's live session.
            return
        }
        daemonDriven = true
        val covered = LinkedHashMap<String, Boolean>()
        val consumed = LinkedHashMap<String, Boolean>()
        val driver = ParityFakeDriver { surface, observable, body ->
            covered[surface] = true
            consumed[observable] = true
            try {
                body()
            } catch (t: Throwable) {
                daemonFailures.add(
                    ParityFailure(surface, "fake-daemon", t.message ?: t.toString())
                )
            }
        }
        val registered = LinkedHashMap(ParityMatrixRegistry.observables)
        try {
            ParityMatrixRegistry.current = driver
            for (row in rows) {
                row.daemonCheck()
            }
        } catch (t: Throwable) {
            daemonFailures.add(
                ParityFailure("fake-daemon", "suite", t.message ?: t.toString())
            )
        } finally {
            ParityMatrixRegistry.current = null
        }
        for (key in registered.keys) {
            if (consumed[key] != true) {
                daemonFailures.add(
                    ParityFailure("fake-daemon", "suite", "stale daemon observable '$key'")
                )
            }
        }
        for (row in rows) {
            val alreadyFailed = daemonFailures.any { it.surface == row.surface }
            if (covered[row.surface] != true && !alreadyFailed) {
                daemonFailures.add(
                    ParityFailure(
                        row.surface,
                        "fake-daemon",
                        "the fake-daemon suite never drove this surface (stale row)"
                    )
                )
            }
        }
    }
    // ----------------------------------------------------------- artifact

    private fun failures(): List<ParityFailure> = cannedFailures + daemonFailures

    /** Behavioral failure count (0 == every row passed both checks). */
    fun behavioralFailures(): Int = failures().size

    /**
     * Runs the whole matrix once per JVM. The fake-daemon suite calls this
     * inside its live session (the driver needs the daemon); a call without
     * that suite records every daemon row as unproven rather than pretending
     * they passed. Returns the visual results.
     */
    fun run(): List<ParityVisualResult> {
        if (!registered) {
            registered = true
            registerRows()
        }
        ranLocally = true
        runCanned()
        if (!daemonDriven) {
            val registered = LinkedHashMap(ParityMatrixRegistry.observables)
            if (registered.isEmpty()) {
                // No suite ran: every daemon row is unproven, never passed.
                for (row in rows) {
                    daemonFailures.add(
                        ParityFailure(
                            row.surface,
                            "fake-daemon",
                            "the fake-daemon suite did not run (no observables registered)"
                        )
                    )
                }
                daemonDriven = true
            } else {
                runDaemon()
            }
        }
        if (!finished) {
            finished = true
            visualResults = runVisual()
            writeArtifact(visualResults)
        }
        return visualResults
    }

    /** True when this JVM ran the matrix (the artifact is on disk). */
    fun ranLocally(): Boolean = ranLocally

    /** The visual results of the run (empty before [`run`]). */
    fun visuals(): List<ParityVisualResult> = visualResults

    // ------------------------------------------------------------- rows

    private fun registerRows() {
        register(
            ParityRow(
                "task mode",
                canned = {
                    // Observable: the strict `POST …/task-runs` body carries
                    // goal + criteria + mutation_mode, and the Task tab is the
                    // only place a completion contract is checked.
                    assertEquals(
                        "{\"goal\":\"g\",\"criteria\":[\"c\"],\"mutation_mode\":\"shadow\"}",
                        NativeRequests.startTaskRun("g", listOf("c"), mutationMode = "shadow")
                    )
                    assertEquals("{\"goal\":\"g\"}", NativeRequests.startTaskRun("g"))
                    val panel = FaktorChatPanel(
                        FaktorFrontendService(
                            java.nio.file.Paths.get("unused-binary"),
                            java.nio.file.Paths.get(
                                System.getProperty("java.io.tmpdir"), "faktor-parity-task"
                            )
                        )
                    )
                    try {
                        assertTrue(panel.tabTitles().contains("Task"), panel.tabTitles().toString())
                        assertEquals(null, panel.completionContractFromControls())
                        panel.completionCommit.isSelected = true
                        panel.completionPr.isSelected = true
                        assertEquals(
                            NativeCompletionContract(true, false, true),
                            panel.completionContractFromControls()
                        )
                    } finally {
                        panel.shutdown()
                    }
                },
                daemonCheck = { fakeDaemonCheck("task mode", "task_runs") }
            )
        )
        register(
            ParityRow(
                "agent tree",
                canned = {
                    // Observable: child identity/state/blocker/presentation on
                    // the shared model + the rendered child label and the
                    // deterministic pixel identity.
                    val agents = parseNativeAgents(PARITY_AGENTS_JSON)
                    assertEquals(2, agents.size)
                    val child = agents[1]
                    assertEquals("Blocked", child.state)
                    assertEquals("permission", child.blocker?.kind)
                    assertEquals("background", child.presentation)
                    val foreground = child.copy(agentId = "child-2", presentation = "foreground")
                    val model = TaskTree.build(agents = listOf(child, foreground))
                    assertEquals(listOf("child-2", "child-1"), model.children.map { it.childId })
                    assertEquals(true, model.children[1].background)
                    val tree = TaskTreePanel()
                    tree.update(model)
                    val label = tree.childLabel(model.children[1])
                    for (needle in listOf("child-1", "[Blocked]", "blocker=permission", "(background)")) {
                        assertTrue(label.contains(needle), "$needle missing: $label")
                    }
                    assertEquals(PixelAgents.avatar("child-1"), PixelAgents.avatar("child-1"))
                    assertEquals(PixelState.BLOCKED, PixelAgents.stateOf("Blocked"))
                    assertEquals(PixelState.RUNNING, PixelAgents.stateOf("Running"))
                },
                daemonCheck = { fakeDaemonCheck("agent tree", "agents") }
            )
        )
        register(
            ParityRow(
                "criterion proofs",
                canned = {
                    // Observable: every binding kind the wire serves renders
                    // with its exact reference; unavailable never renders pass;
                    // a legacy unbound row derives honestly.
                    val model = criterionProofModel()
                    val labels = TaskTreePanel().criterionLabels(model)
                    assertEquals(PARITY_BINDING_KINDS.size + 2, model.criteriaProof.size)
                    for ((criterion, kind, reference) in PARITY_BINDING_KINDS) {
                        val row = model.criteriaProof.firstOrNull { it.criterionKey == criterion }
                            ?: fail("missing proof row $criterion")
                        assertEquals(kind, row.bindingKind, criterion)
                        assertEquals("daemon", row.bindingSource, criterion)
                        assertEquals(reference, row.bindingReference, criterion)
                    }
                    val unavailable = model.criteriaProof
                        .first { it.criterionKey == "unavailable criterion" }
                    assertEquals("unavailable", unavailable.bindingKind)
                    assertEquals(CriterionVerdictTone.UNAVAILABLE, unavailable.tone)
                    val legacy = model.criteriaProof.first { it.criterionKey == "legacy criterion" }
                    assertEquals("derived", legacy.bindingSource)
                    assertEquals("file_state", legacy.bindingKind)
                    assertTrue(
                        labels.size == model.criteriaProof.size &&
                            labels.any { it.contains("binding required_check") },
                        "proof rows render their binding kinds: $labels"
                    )
                },
                daemonCheck = { fakeDaemonCheck("criterion proofs", "criterion_proofs") }
            )
        )
        register(
            ParityRow(
                "permissions",
                canned = {
                    // Observable: unavailable state is never an empty list;
                    // the pending list renders capability+detail; a reply
                    // routes the strict DTO.
                    val permissions = parseNativePermissionList(PARITY_PERMISSION_LIST_JSON)
                    val panel = PermissionsPanel()
                    var repliedId: String? = null
                    var repliedDecision: String? = null
                    panel.setListener(object : PermissionsPanel.Listener {
                        override fun onPermissionReply(
                            permission: dev.faktor.shared.NativePermissionEntry,
                            decision: String
                        ) {
                            repliedId = permission.id
                            repliedDecision = decision
                        }

                        override fun onRefresh() {}
                    })
                    assertEquals(false, panel.allowEnabled())
                    panel.setUnavailable("no permission route (status 404 not_found)")
                    assertEquals(false, panel.available())
                    panel.update(permissions)
                    assertEquals(1, panel.count())
                    assertTrue(panel.unitLabel(0).contains("capability=shell"), panel.unitLabel(0))
                    panel.submitReply("deny")
                    assertEquals("7", repliedId)
                    assertEquals("deny", repliedDecision)
                    assertEquals(
                        "{\"permission_id\":\"7\",\"decision\":\"allow\"}",
                        NativeRequests.permissionReply("7", "allow")
                    )
                },
                daemonCheck = { fakeDaemonCheck("permissions", "permissions") }
            )
        )
        register(
            ParityRow(
                "terminal",
                canned = {
                    // Observable: session-owned rows + unowned count, spawn
                    // body shape, durable lifetime events, output routing.
                    val page = parseNativeTerminalPage(PARITY_TERMINALS_JSON)
                    assertEquals("7", page.sessionId)
                    assertEquals("5", page.terminals[0].id)
                    assertEquals(true, page.terminals[0].alive)
                    assertEquals("3", page.terminals[0].taskId)
                    assertEquals(1L, page.unowned)
                    val events = parseNativeTerminalEventPage(PARITY_TERMINAL_EVENTS_JSON)
                    assertEquals("created", events.events[0].type)
                    assertEquals("6", parseNativeTerminalSpawned(PARITY_TERMINAL_SPAWNED_JSON).ptyId)
                    val panel = TerminalPanel()
                    panel.update(page)
                    panel.setEvents(events)
                    panel.setComposerFields("bash", "-lc echo-parity", "/tmp")
                    assertTrue(panel.terminalLabel(0).contains("pty 5"), panel.terminalLabel(0))
                    assertTrue(panel.eventsText().contains("#1 created"), panel.eventsText())
                    panel.setOutput(parseNativeTerminalOutput("6", PARITY_TERMINAL_OUTPUT_JSON))
                    assertTrue(panel.outputText().contains("pty 6 alive=true"), panel.outputText())
                },
                daemonCheck = { fakeDaemonCheck("terminal", "terminal") }
            )
        )
        register(
            ParityRow(
                "review/tournament",
                canned = {
                    // Observable: candidate review verdicts surface; decide is
                    // gated on every candidate settled; a decided tournament
                    // exposes no decide.
                    val view = TaskTree.tournamentView(parseNativeTournament(PARITY_TOURNAMENT_JSON))
                    val panel = TournamentPanel()
                    panel.setSummaries(parseNativeTournamentSummaries(PARITY_TOURNAMENTS_LIST_JSON))
                    panel.setTournament(view)
                    assertEquals(2, panel.candidateCount())
                    assertEquals(false, panel.decideEnabled())
                    assertEquals("clean", view.candidates[0].reviewRank)
                    assertEquals("rev-1", view.candidates[0].reviewer)
                    val settled = TournamentPanel()
                    settled.setTournament(
                        view.copy(
                            candidates = view.candidates.map { it.copy(state = "done") },
                            winner = null,
                            state = "open"
                        )
                    )
                    assertEquals(true, settled.decideEnabled())
                    val pending = view.copy(
                        candidates = view.candidates.mapIndexed { index, candidate ->
                            if (index == 1) candidate.copy(state = "running") else candidate
                        }
                    )
                    val pendingPanel = TournamentPanel()
                    pendingPanel.setTournament(pending)
                    assertEquals(false, pendingPanel.decideEnabled())
                },
                daemonCheck = { fakeDaemonCheck("review/tournament", "tournament") }
            )
        )
        register(
            ParityRow(
                "evidence",
                canned = {
                    // Observable: `evidence:<n>` refs parse from free text; the
                    // navigator serves refs + transcript jumps + the strict
                    // selector DTO.
                    val refs = listOf(
                        EvidenceRefs.parse("evidence:41"),
                        EvidenceRefs.parse("see evidence/42 here")
                    )
                    assertEquals(41L, refs[0].id)
                    assertEquals(42L, refs[1].id)
                    val panel = EvidenceNavigatorPanel()
                    panel.setEvidence(refs)
                    panel.setMessages(
                        listOf(
                            NativeMessage(seq = 1, id = 1, role = "user", createdMs = 1, text = "hi"),
                            NativeMessage(seq = 2, id = 2, role = "assistant", createdMs = 2, text = "hello")
                        )
                    )
                    assertEquals(2, panel.evidenceCount())
                    panel.selectEvidence(refs[0])
                    assertEquals("{\"selector\":\"all\"}", panel.selectorJson())
                    assertEquals(
                        "{\"selector\":\"search\",\"query\":\"needle\",\"max_hits\":3}",
                        NativeRequests.evidenceSelectorSearch("needle", 3L)
                    )
                },
                daemonCheck = { fakeDaemonCheck("evidence", "evidence") }
            )
        )
        register(
            ParityRow(
                "settings",
                canned = {
                    // Observable: daemon info + mutation-mode state + the
                    // honest unavailable state.
                    val panel = SettingsPanel()
                    panel.setDaemonInfo(
                        "/bin/faktor-cli", "/tmp/data", "1.2.3", "http://127.0.0.1:9"
                    )
                    assertTrue(panel.daemonText().contains("/bin/faktor-cli"), panel.daemonText())
                    assertEquals(listOf("default", "shadow"), panel.mutationModes())
                    assertEquals(null, panel.mutationMode())
                    // Shadow-only: the removed direct-owner mode cannot be
                    // selected and never reads back.
                    panel.selectMutationMode("direct_compat")
                    assertEquals(null, panel.mutationMode())
                    panel.selectMutationMode("shadow")
                    assertEquals("shadow", panel.mutationMode())
                    panel.setUnavailable("provider read failed: daemon down")
                    assertEquals(false, panel.available())
                },
                daemonCheck = { fakeDaemonCheck("settings", "settings") }
            )
        )
        register(
            ParityRow(
                "provider selection",
                canned = {
                    // Observable: (provider, model) is the only catalog join
                    // key; two providers sharing a model id never merge.
                    val catalog = parseNativeModelCatalog(PARITY_DUAL_MODELS_JSON)
                    val panel = SettingsPanel()
                    panel.setCatalog(catalog)
                    assertEquals(2, panel.providerCount())
                    assertEquals("alpha", panel.selectedProvider())
                    panel.selectModel("m")
                    assertEquals("alpha/m", "${panel.selectedProvider()}/${panel.selectedModel()}")
                    panel.selectProvider("beta")
                    assertEquals("beta/m", "${panel.selectedProvider()}/${panel.selectedModel()}")
                    val child = parseNativeAgents(PARITY_AGENTS_JSON)[1]
                    val model = TaskTree.build(
                        agents = listOf(
                            child.copy(provider = "alpha"),
                            child.copy(agentId = "child-b", provider = "beta")
                        ),
                        catalog = catalog
                    )
                    assertEquals(true, model.children[0].reasoning)
                    assertEquals(false, model.children[1].reasoning)
                },
                daemonCheck = { fakeDaemonCheck("provider selection", "provider_selection") }
            )
        )
        register(
            ParityRow(
                "history",
                canned = {
                    // Observable: durable session list + current marker + open
                    // routing + the stream cursor read.
                    val sessions = parseNativeSessionList(PARITY_SESSIONS_JSON)
                    val panel = HistoryPanel()
                    panel.setUnavailable("history read refused (status 503)")
                    assertEquals(false, panel.available())
                    panel.update(sessions, "7")
                    assertEquals(2, panel.count())
                    assertTrue(panel.label(0).contains("(current)"), panel.label(0))
                    panel.select(1)
                    assertEquals("8", panel.selectedId())
                    panel.setConnection("connected: http://127.0.0.1:9", "open", 42L, "7")
                    assertTrue(panel.streamText().contains("cursor=42"), panel.streamText())
                },
                daemonCheck = { fakeDaemonCheck("history", "history") }
            )
        )
        register(
            ParityRow(
                "restart/reconnect",
                canned = {
                    // Observable: the restart/reconnect controls exist on the
                    // durable history surface; a stopped connection renders
                    // honestly (the real daemon drives the live behavior).
                    val panel = HistoryPanel()
                    panel.update(parseNativeSessionList(PARITY_SESSIONS_JSON), "7")
                    assertEquals(true, panel.restartEnabled())
                    assertEquals(true, panel.reconnectEnabled())
                    panel.setConnection("stopped", "off", 0L, null)
                    assertTrue(panel.streamText().contains("off"), panel.streamText())
                },
                daemonCheck = { fakeDaemonCheck("restart/reconnect", "restart_reconnect") }
            )
        )
    }

    // ------------------------------------------------------------- visual

    /**
     * Renders every panel into an offscreen `BufferedImage`, computes a
     * canonical component-tree/state digest and compares it against the pinned
     * baseline. A missing baseline is reported as unproven (never a pass);
     * `-Dfaktor.parity.writeBaselines=true` writes/refreshes the pin.
     */
    private fun runVisual(): List<ParityVisualResult> {
        val panels = listOf(
            "task-tree" to cannedTaskTreePanel(),
            "blockers" to cannedBlockersPanel(),
            "tournament" to cannedTournamentPanel(),
            "permissions" to cannedPermissionsPanel(),
            "terminal" to cannedTerminalPanel(),
            "evidence" to cannedEvidencePanel(),
            "settings" to cannedSettingsPanel(),
            "history" to cannedHistoryPanel()
        )
        val digests = LinkedHashMap<String, String>()
        for ((name, panel) in panels) {
            val render = ParityAwt.render(panel)
            val renderOk = render.width > 0 && render.height > 0 &&
                render.distinctColors >= 4 &&
                render.nonCornerPermille in 1..999
            assertEquals(true, renderOk, "$name rendered a degenerate frame: $render")
            digests[name] = ParityAwt.digest(panel, name)
        }
        val baselineFile = File(ParityPath.repoRoot(), PARITY_BASELINE_PATH)
        if (System.getProperty("faktor.parity.writeBaselines") == "true") {
            writeVisualBaselines(baselineFile, digests)
            return digests.map { (name, digest) ->
                ParityVisualResult(
                    name, true, digest, digest, "baseline pinned from this render"
                )
            }
        }
        val pinned = readVisualBaselines(baselineFile)
        return digests.map { (name, digest) ->
            val expected = pinned?.get(name)
            when {
                pinned == null -> ParityVisualResult(
                    name, false, digest, null,
                    "no pinned visual baseline at $PARITY_BASELINE_PATH"
                )
                expected == null -> ParityVisualResult(
                    name, false, digest, null, "panel $name missing from the pinned baseline"
                )
                expected != digest -> ParityVisualResult(
                    name, false, digest, expected,
                    "component-tree/state digest drifted from the pinned baseline"
                )
                else -> ParityVisualResult(
                    name, true, digest, expected, "digest matches the pinned baseline"
                )
            }
        }
    }

    private fun readVisualBaselines(file: File): Map<String, String>? {
        if (!file.isFile) return null
        val json = try {
            JsonCodec.parse(file.readText(Charsets.UTF_8))
        } catch (e: Exception) {
            return null
        }
        val field = json.view("baseline").field("panelDigests").value as? JsonValue.Obj
            ?: return null
        val out = LinkedHashMap<String, String>()
        for ((key, value) in field.fields) {
            out[key] = (value as? JsonValue.Str)?.value ?: return null
        }
        return out
    }

    private fun writeVisualBaselines(file: File, digests: Map<String, String>) {
        val expected = File(ParityPath.repoRoot(), PARITY_BASELINE_PATH)
        assertEquals(
            expected.canonicalPath, file.canonicalPath,
            "visual baselines must be written to the pinned in-tree path"
        )
        file.parentFile.mkdirs()
        val out = StringBuilder()
        out.append("{\n")
        out.append("  \"schema\": \"faktor-parity-visual-baselines/v1\",\n")
        out.append("  \"method\": ")
        JsonCodec.writeString(out, VISUAL_METHOD)
        out.append(",\n")
        out.append("  \"panelDigests\": {\n")
        val entries = digests.entries.toList()
        for ((index, entry) in entries.withIndex()) {
            out.append("    ")
            JsonCodec.writeString(out, entry.key)
            out.append(": ")
            JsonCodec.writeString(out, entry.value)
            out.append(if (index == entries.size - 1) "\n" else ",\n")
        }
        out.append("  }\n")
        out.append("}\n")
        file.writeText(out.toString(), Charsets.UTF_8)
    }

    // ----------------------------------------------------------- artifact

    private fun writeArtifact(visual: List<ParityVisualResult>) {
        val commit = ParityPath.headCommit() ?: "unknown"
        val allFailures = failures()
        val jsonRows = ArrayList<JsonValue>()
        var passed = 0
        for (row in rows) {
            val rowFailures = allFailures.filter { it.surface == row.surface }
            val ok = rowFailures.isEmpty()
            if (ok) passed++
            val evidence = if (ok) {
                "canned native frames: all observables matched; fake daemon: all observables matched"
            } else {
                rowFailures.joinToString("; ") { "${it.check}: ${it.message}" }
            }
            jsonRows.add(
                obj(
                    "surface" to JsonValue.Str(row.surface),
                    "status" to JsonValue.Str(if (ok) "passed" else "failed"),
                    "evidence" to JsonValue.Str(evidence)
                )
            )
        }
        val visualPassed = visual.count { it.passed }
        val visualComplete = visual.isNotEmpty() && visualPassed == visual.size
        val artifact = obj(
            "schema" to JsonValue.Str("faktor-jetbrains-parity/v1"),
            "commit" to JsonValue.Str(commit),
            "generated_from" to JsonValue.Str(
                "apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParityMatrix.kt"
            ),
            "status" to JsonValue.Str(
                if (passed == rows.size && visualComplete) "passed" else "partial"
            ),
            "rows" to JsonValue.Arr(jsonRows),
            "behavioral" to obj(
                "passed" to JsonValue.Int64(passed.toLong()),
                "total" to JsonValue.Int64(rows.size.toLong())
            ),
            "visual" to obj(
                "status" to JsonValue.Str(
                    when {
                        visual.isEmpty() -> "unavailable"
                        visualComplete -> "passed"
                        else -> "failed"
                    }
                ),
                "passed" to JsonValue.Int64(visualPassed.toLong()),
                "total" to JsonValue.Int64(visual.size.toLong()),
                "method" to JsonValue.Str(VISUAL_METHOD),
                "baseline" to JsonValue.Str(PARITY_BASELINE_PATH),
                "panels" to JsonValue.Arr(
                    visual.map { result ->
                        obj(
                            "panel" to JsonValue.Str(result.panel),
                            "status" to JsonValue.Str(if (result.passed) "passed" else "failed"),
                            "digest" to JsonValue.Str(result.digest),
                            "baseline" to (result.baseline?.let { JsonValue.Str(it) } ?: JsonValue.Null),
                            "detail" to JsonValue.Str(result.detail)
                        )
                    }
                )
            )
        )
        val file = File(ParityPath.repoRoot(), PARITY_PATH)
        file.parentFile.mkdirs()
        file.writeText(JsonCodec.write(artifact) + "\n", Charsets.UTF_8)
        println(
            "JETBRAINS PARITY MATRIX: ${rows.size} behavioral rows ($passed passed), " +
                "visual $visualPassed/${visual.size} panels, artifact $PARITY_PATH @ $commit"
        )
    }
}

/** The fake-daemon row driver: the suite submits one check per surface. */
internal fun interface ParityFakeDriver {
    fun check(surface: String, observable: String, body: () -> Unit)
}

/**
 * Submits the fake-daemon observable one surface row names. The suite owns
 * the daemon; the row speaks only in (surface, observable key).
 */
internal fun fakeDaemonCheck(surface: String, observable: String) {
    val driver = ParityMatrixRegistry.current
        ?: fail("the fake-daemon suite is not running (row $surface)")
    val body = ParityMatrixRegistry.observables[observable]
        ?: fail("no fake-daemon observable named '$observable' for row $surface")
    driver.check(surface, observable, body)
}

/** The daemon-suite observables, keyed by name, for the current run. */
internal object ParityMatrixRegistry {
    @Volatile var current: ParityFakeDriver? = null
    val observables = LinkedHashMap<String, () -> Unit>()

    fun reset() {
        current = null
        observables.clear()
    }
}

// ------------------------------------------------------------- visual engine

/** Path helpers: repo-root discovery, HEAD commit, sha-256 hex. */
internal object ParityPath {
    fun repoRoot(): File {
        val prop = System.getProperty("faktor.repo.root")
        if (!prop.isNullOrEmpty()) {
            val root = File(prop).absoluteFile
            assertTrue(
                File(root, "Cargo.toml").isFile && File(root, "crates").isDirectory,
                "faktor.repo.root is not a Faktor repository root: $root"
            )
            return root
        }
        var dir: File? = File(".").absoluteFile
        var guard = 0
        while (dir != null && guard < 8) {
            if (File(dir, "Cargo.toml").isFile && File(dir, "crates").isDirectory) return dir
            dir = dir.parentFile
            guard++
        }
        fail("cannot locate the repository root (Cargo.toml + crates/); pass -Dfaktor.repo.root")
    }

    fun headCommit(): String? {
        return try {
            val process = ProcessBuilder("git", "rev-parse", "HEAD")
                .directory(repoRoot())
                .redirectErrorStream(true)
                .start()
            val text = process.inputStream.bufferedReader().readText().trim()
            if (process.waitFor() == 0 && text.isNotEmpty()) text else null
        } catch (e: Exception) {
            null
        }
    }

    fun sha256Hex(bytes: ByteArray): String {
        val digest = MessageDigest.getInstance("SHA-256").digest(bytes)
        val sb = StringBuilder(digest.size * 2)
        for (byte in digest) {
            val value = byte.toInt() and 0xff
            sb.append(Character.forDigit(value ushr 4, 16))
            sb.append(Character.forDigit(value and 0x0f, 16))
        }
        return sb.toString()
    }
}

/** The offscreen render + canonical state-digest machinery (no IDE needed). */
internal object ParityAwt {

    private const val MAX_DEPTH = 64
    private val URL_PATTERN = Regex("http://127\\.0\\.0\\.1:\\d+")

    /**
     * Gives the panel its deterministic surface (fixed size + one layout
     * pass) and renders it offscreen. The layout pass is what makes the
     * component-tree digest see real bounds on every machine.
     */
    fun render(panel: Component): RenderStats {
        val width = 900
        val height = 600
        layout(panel, width, height)
        val image = BufferedImage(width, height, BufferedImage.TYPE_INT_RGB)
        val g = image.createGraphics()
        try {
            panel.paint(g)
        } finally {
            g.dispose()
        }
        val corner = image.getRGB(0, 0)
        val distinct = LinkedHashMap<Int, Boolean>()
        var nonCorner = 0
        var sampled = 0
        var y = 0
        while (y < height) {
            var x = 0
            while (x < width) {
                val rgb = image.getRGB(x, y)
                distinct[rgb] = true
                sampled++
                if (rgb != corner) nonCorner++
                x += 16
            }
            y += 16
        }
        val permille = if (sampled == 0) 0 else nonCorner * 1000 / sampled
        return RenderStats(width, height, distinct.size, permille)
    }

    /** Fixed-size layout pass over the whole tree (digest == rendered state). */
    fun layout(panel: Component, width: Int, height: Int) {
        panel.setSize(width, height)
        if (panel is Container) {
            panel.doLayout()
            layoutChildren(panel)
        }
    }

    private fun layoutChildren(container: Container) {
        for (child in container.components) {
            if (child is Container) {
                child.doLayout()
                layoutChildren(child)
            }
        }
    }

    /**
     * Canonical component-tree/state digest: class names + names + layout
     * state + bounds + colors + text as served. URL literals are normalized
     * (an ephemeral port is never part of a panel's identity), and fonts are
     * deliberately excluded so the digest pins BEHAVIOR/STRUCTURE, not the
     * host's font rendering.
     */
    fun digest(panel: Component, name: String): String {
        val sb = StringBuilder()
        sb.append("panel ").append(name).append('\n')
        walk(panel, sb, 0)
        return ParityPath.sha256Hex(sb.toString().toByteArray(Charsets.UTF_8))
    }

    private fun walk(component: Component, sb: StringBuilder, depth: Int) {
        if (depth > MAX_DEPTH) {
            sb.append("  ".repeat(depth)).append("depth-limit\n")
            return
        }
        val indent = "  ".repeat(depth)
        sb.append(indent).append(component.javaClass.name)
        sb.append(" name=").append(normalize(component.name ?: "-"))
        sb.append(" bounds=").append(component.x).append(',').append(component.y)
            .append(',').append(component.width).append(',').append(component.height)
        sb.append(" visible=").append(component.isVisible)
        sb.append(" enabled=").append(component.isEnabled)
        sb.append(" opaque=").append(component.isOpaque)
        component.background?.let { sb.append(" bg=#").append(hex(it.rgb)) }
        component.foreground?.let { sb.append(" fg=#").append(hex(it.rgb)) }
        when (component) {
            is javax.swing.AbstractButton -> {
                sb.append(" text=").append(normalize(component.text ?: ""))
                sb.append(" selected=").append(component.isSelected)
            }
            is javax.swing.JLabel -> sb.append(" text=").append(normalize(component.text ?: ""))
            is javax.swing.text.JTextComponent ->
                sb.append(" text=").append(normalize(component.text ?: ""))
            is javax.swing.JComboBox<*> -> {
                sb.append(" selected=").append(normalize(component.selectedItem?.toString() ?: "-"))
                sb.append(" items=").append(
                    normalize(
                        (0 until component.itemCount).joinToString("|") {
                            component.getItemAt(it)?.toString() ?: "-"
                        }
                    )
                )
            }
            is javax.swing.JList<*> -> {
                sb.append(" items=").append(
                    normalize(
                        (0 until component.model.size).joinToString("|") {
                            component.model.getElementAt(it)?.toString() ?: "-"
                        }
                    )
                )
                sb.append(" selected=").append(component.selectedIndex)
            }
            is javax.swing.JTree -> sb.append(" rows=").append(component.rowCount)
            is javax.swing.JTabbedPane -> {
                sb.append(" tabs=").append(
                    normalize(
                        (0 until component.tabCount).joinToString("|") { component.getTitleAt(it) }
                    )
                )
                sb.append(" selected=").append(component.selectedIndex)
            }
            is javax.swing.JSpinner -> sb.append(" value=").append(component.value)
        }
        val container = component as? Container
        if (container != null) {
            sb.append(" children=").append(container.componentCount).append('\n')
            for (child in container.components) {
                walk(child, sb, depth + 1)
            }
        } else {
            sb.append('\n')
        }
    }

    private fun normalize(text: String): String =
        URL_PATTERN.replace(text.replace("\r\n", "\n"), "http://127.0.0.1:<port>")

    private fun hex(rgb: Int): String = String.format("%06x", rgb and 0xffffff)
}

internal data class RenderStats(
    val width: Int,
    val height: Int,
    val distinctColors: Int,
    val nonCornerPermille: Int
) {
    override fun toString(): String =
        "image=${width}x$height colors=$distinctColors nonCornerPermille=$nonCornerPermille"
}

// ------------------------------------------------------------- visual data

internal const val VISUAL_METHOD =
    "offscreen-swing-render+component-tree-state-digest-vs-pinned-baseline"

/** All seven typed criterion-binding kinds, with their exact references. */
internal val PARITY_BINDING_KINDS: List<Triple<String, String, String?>> = listOf(
    Triple("check criterion", "required_check", "check:rust_check:digest-check"),
    Triple("coverage criterion", "integration_coverage", "work-item:impl-a, work-item:impl-b"),
    Triple("file criterion", "file_state", "src/a.rs"),
    Triple("evidence criterion", "evidence", "evidence:41"),
    Triple("review criterion", "independent_review", "reviewer-1"),
    Triple("aggregate criterion", "aggregate_goal", null),
    Triple("unavailable criterion", "unavailable", null)
)

internal fun criterionProofModel(): TaskTreeModel = TaskTree.build(
    task = parseNativeTaskViews(PARITY_TASK_JSON)[0],
    taskVerification = parseNativeTaskVerification(PARITY_CRITERION_PROOF_JSON)
)

internal fun cannedTaskTreePanel(): TaskTreePanel {
    val panel = TaskTreePanel()
    panel.update(criterionProofModel())
    return panel
}

internal fun cannedBlockersPanel(): BlockersPanel {
    val tree = TaskTree.build(
        agents = parseNativeAgents(PARITY_AGENTS_JSON),
        catalog = parseNativeModelCatalog(PARITY_MODELS_JSON)
    )
    // One dependency-blocked child on top of the permission-blocked one: the
    // blockers surface renders every kind, not just permissions.
    val dependency = TaskTree.build(
        agents = listOf(parseNativeAgents(PARITY_AGENTS_DEPENDENCY_JSON)[1])
    )
    val model = tree.copy(blockers = tree.blockers + dependency.blockers)
    val panel = BlockersPanel()
    panel.update(model.blockers, parseNativePermissionList(PARITY_PERMISSION_LIST_JSON), model.taskBlockers)
    return panel
}

internal fun cannedTournamentPanel(): TournamentPanel {
    val panel = TournamentPanel()
    panel.setSummaries(parseNativeTournamentSummaries(PARITY_TOURNAMENTS_LIST_JSON))
    panel.setTournament(TaskTree.tournamentView(parseNativeTournament(PARITY_TOURNAMENT_JSON)))
    return panel
}

internal fun cannedPermissionsPanel(): PermissionsPanel {
    val panel = PermissionsPanel()
    panel.update(parseNativePermissionList(PARITY_PERMISSION_LIST_JSON))
    return panel
}

internal fun cannedTerminalPanel(): TerminalPanel {
    val panel = TerminalPanel()
    panel.update(parseNativeTerminalPage(PARITY_TERMINALS_JSON))
    panel.setEvents(parseNativeTerminalEventPage(PARITY_TERMINAL_EVENTS_JSON))
    panel.setOutput(parseNativeTerminalOutput("5", PARITY_TERMINAL_OUTPUT_JSON))
    return panel
}

internal fun cannedEvidencePanel(): EvidenceNavigatorPanel {
    val panel = EvidenceNavigatorPanel()
    val refs = listOf(EvidenceRefs.parse("evidence:41"), EvidenceRefs.parse("see evidence/42 here"))
    panel.setEvidence(refs)
    panel.setMessages(
        listOf(
            NativeMessage(seq = 1, id = 1, role = "user", createdMs = 1, text = "hi"),
            NativeMessage(seq = 2, id = 2, role = "assistant", createdMs = 2, text = "hello")
        )
    )
    panel.selectEvidence(refs[0])
    panel.showRetrieval(41L, "evidence:41 tool output", 12L, false)
    return panel
}

internal fun cannedSettingsPanel(): SettingsPanel {
    val panel = SettingsPanel()
    panel.setDaemonInfo("/bin/faktor-cli", "/tmp/data", "1.2.3", "http://127.0.0.1:9")
    panel.setCatalog(parseNativeModelCatalog(PARITY_DUAL_MODELS_JSON))
    panel.setProviders(parseNativeProviders(PARITY_PROVIDERS_JSON))
    return panel
}

internal fun cannedHistoryPanel(): HistoryPanel {
    val panel = HistoryPanel()
    panel.update(parseNativeSessionList(PARITY_SESSIONS_JSON), "7")
    panel.select(1)
    panel.setConnection("attached port 9 version fake-1", "open", 42L, "7")
    return panel
}

// --------------------------------------------------------------- fixtures

internal const val PARITY_AGENTS_JSON = "[" +
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

internal const val PARITY_AGENTS_DEPENDENCY_JSON = "[" +
    "{\"agent_id\":\"self-1\",\"kind\":\"self\",\"run_id\":\"run-9\"," +
    "\"session_id\":7,\"worktree_id\":1,\"goal\":\"ship it\",\"state\":\"Running\"," +
    "\"model\":\"m\",\"budget\":null,\"ownership\":\"self\",\"item_ids\":[\"main\"]," +
    "\"progress\":null,\"result\":null}," +
    "{\"agent_id\":\"child-2\",\"kind\":\"child\",\"run_id\":\"run-9\"," +
    "\"session_id\":10,\"worktree_id\":3,\"goal\":\"wait for impl\",\"state\":\"Blocked\"," +
    "\"model\":\"m\",\"provider\":\"beta\",\"budget\":2000,\"ownership\":\"ReadOnly\"," +
    "\"item_id\":\"verify\",\"item_kind\":\"Verification\"," +
    "\"blocker\":{\"kind\":\"dependency\",\"reason\":\"waiting on impl-a\"," +
    "\"dependency\":\"impl-a\",\"resolution\":\"wait for impl-a\"," +
    "\"last_progress_ms\":7}," +
    "\"progress\":null,\"result\":null}]"

internal const val PARITY_TASK_JSON = "[" +
    "{\"goal\":\"parity goal\",\"constraints\":[],\"state\":\"in_progress\"," +
    "\"milestones\":{\"completed\":[],\"open\":[]}," +
    "\"decisions\":[],\"failures\":[],\"changedFiles\":[]," +
    "\"tests\":{\"run\":[],\"failed\":[]},\"preferences\":[],\"verification\":[]," +
    "\"progress\":null,\"budget\":null," +
    "\"acceptanceCriteria\":[\"criterion A\"]," +
    "\"plan\":[" +
    "{\"id\":\"step-1\",\"summary\":\"first\",\"state\":\"done\",\"depends_on\":[]}," +
    "{\"id\":\"step-2\",\"summary\":\"second\",\"state\":\"running\"," +
    "\"depends_on\":[\"step-1\"]}]," +
    "\"blockers\":[],\"evidenceRefs\":[],\"phase\":\"building\"}]"

/**
 * Criterion proof coverage: all seven typed binding kinds + the honest
 * unavailable + a legacy unbound row the model derives from.
 */
internal const val PARITY_CRITERION_PROOF_JSON = "{" +
    "\"sessionId\":\"7\",\"taskId\":\"3\",\"records\":[{" +
    "\"recordId\":\"r1\",\"revision\":\"rev\",\"workspaceId\":\"1\",\"worktreeId\":\"1\"," +
    "\"treeHash\":null," +
    "\"criteria\":[" +
    "{\"criterionKey\":\"check criterion\",\"passed\":true," +
    "\"evidence\":\"check:rust_check:digest-check\"," +
    "\"binding\":{\"kind\":\"required_check\",\"check_id\":\"rust_check\"," +
    "\"command_digest\":\"digest-check\"}," +
    "\"origin\":\"user\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"coverage criterion\",\"passed\":true,\"evidence\":null," +
    "\"binding\":{\"kind\":\"integration_coverage\"," +
    "\"required_work_items\":[\"impl-a\",\"impl-b\"]}," +
    "\"origin\":\"project_policy\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"file criterion\",\"passed\":true,\"evidence\":\"file:src/a.rs\"," +
    "\"binding\":{\"kind\":\"file_state\",\"path\":\"src/a.rs\"," +
    "\"expected_digest\":\"digest-file\"}," +
    "\"origin\":\"verification_policy\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"evidence criterion\",\"passed\":true,\"evidence\":\"evidence:41\"," +
    "\"binding\":{\"kind\":\"evidence\",\"evidence_id\":\"41\"," +
    "\"evidence_digest\":\"digest-evidence\"}," +
    "\"origin\":\"user\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"review criterion\",\"passed\":true,\"evidence\":null," +
    "\"binding\":{\"kind\":\"independent_review\",\"reviewer_id\":\"reviewer-1\"}," +
    "\"origin\":\"project_policy\",\"requirement\":\"preferred\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"aggregate criterion\",\"passed\":true,\"evidence\":null," +
    "\"binding\":{\"kind\":\"aggregate_goal\"}," +
    "\"origin\":\"verification_policy\",\"requirement\":\"required\",\"verdict\":\"pass\"}," +
    "{\"criterionKey\":\"unavailable criterion\",\"passed\":false,\"evidence\":null," +
    "\"binding\":{\"kind\":\"unavailable\",\"reason\":\"no objective mechanism\"}," +
    "\"origin\":\"semantic_provider\",\"requirement\":\"required\"," +
    "\"verdict\":\"unavailable\"}," +
    "{\"criterionKey\":\"legacy criterion\",\"passed\":true," +
    "\"evidence\":\"file:src/legacy.rs\"}" +
    "]," +
    "\"checks\":[],\"changedFiles\":[],\"unrelatedChanges\":[],\"reviewer\":null," +
    "\"status\":\"passed\",\"startedMs\":11,\"completedMs\":22" +
    "}]}"

internal const val PARITY_SESSIONS_JSON = "{\"sessions\":[" +
    "{\"id\":\"7\",\"title\":\"parity\",\"provider\":\"alpha\",\"model\":\"m\"," +
    "\"state\":\"ready\"}," +
    "{\"id\":\"8\",\"title\":\"older\",\"provider\":\"beta\",\"model\":\"n\"," +
    "\"state\":\"ended\"}]}"

internal const val PARITY_MODELS_JSON = "[" +
    "{\"provider\":\"alpha\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
    "\"tools\":true,\"parallelTools\":false,\"reasoning\":true,\"thinking\":false," +
    "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
    "\"streaming\":true,\"source\":\"conservativeDefault\"}," +
    "{\"provider\":\"beta\",\"model\":\"n\",\"context\":2000,\"maxOutput\":200," +
    "\"tools\":false,\"parallelTools\":false,\"reasoning\":false,\"thinking\":true," +
    "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
    "\"streaming\":true,\"source\":\"conservativeDefault\"}]"

internal const val PARITY_DUAL_MODELS_JSON = "[" +
    "{\"provider\":\"alpha\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
    "\"tools\":true,\"parallelTools\":false,\"reasoning\":true,\"thinking\":false," +
    "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
    "\"streaming\":true,\"source\":\"conservativeDefault\"}," +
    "{\"provider\":\"beta\",\"model\":\"m\",\"context\":2000,\"maxOutput\":200," +
    "\"tools\":false,\"parallelTools\":false,\"reasoning\":false,\"thinking\":true," +
    "\"vision\":false,\"structuredOutput\":false,\"embeddings\":false," +
    "\"streaming\":true,\"source\":\"conservativeDefault\"}]"

internal const val PARITY_PROVIDERS_JSON = "[" +
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

internal const val PARITY_PERMISSION_LIST_JSON = "{\"permissions\":[" +
    "{\"id\":\"7\",\"session_id\":\"9\",\"capability\":\"shell\"," +
    "\"detail\":{\"tool\":\"bash\"}}]}"

internal const val PARITY_TERMINALS_JSON = "{" +
    "\"sessionId\":\"7\",\"terminals\":[" +
    "{\"id\":\"5\",\"pid\":123,\"alive\":true,\"sessionId\":\"7\",\"taskId\":\"3\"," +
    "\"agentId\":null,\"operationId\":\"11\",\"spawnedMs\":1700}]," +
    "\"unowned\":1,\"note\":\"1 daemon-level PTY row carries no session ownership\"}"

internal const val PARITY_TERMINAL_EVENTS_JSON = "{" +
    "\"sessionId\":\"7\",\"events\":[" +
    "{\"id\":1,\"type\":\"created\",\"ptyId\":\"5\",\"pid\":123,\"tsMs\":1700," +
    "\"sessionId\":\"7\"}]," +
    "\"hasMore\":false,\"nextCursor\":null}"

internal const val PARITY_TERMINAL_SPAWNED_JSON = "{" +
    "\"ok\":true,\"ptyId\":\"6\",\"pid\":456,\"sessionId\":\"7\",\"taskId\":\"3\"," +
    "\"agentId\":null,\"operationId\":\"12\"}"

internal const val PARITY_TERMINAL_OUTPUT_JSON =
    "{\"ok\":true,\"output\":\"parity-output\\nsecond line\\n\",\"alive\":true}"

internal const val PARITY_BOARD_PAGE_JSON = "{" +
    "\"board_id\":7,\"revision\":0,\"posts\":[]," +
    "\"next_before_revision\":null,\"has_more\":false}"

internal const val PARITY_TOURNAMENTS_LIST_JSON = "[" +
    "{\"id\":\"t-1\",\"state\":\"decided\",\"candidate_count\":2," +
    "\"winner\":\"child-0\",\"decided_ms\":123}," +
    "{\"id\":\"t-2\",\"state\":\"open\",\"candidate_count\":2," +
    "\"winner\":null,\"decided_ms\":null}]"

internal const val PARITY_EVIDENCE_RETRIEVAL_JSON = "{" +
    "\"id\":41,\"selector\":{\"selector\":\"all\"}," +
    "\"bytesBase64\":\"ZXZpZGVuY2U6NDEgcGFyaXR5IHRvb2wgb3V0cHV0\"," +
    "\"byteLen\":29,\"truncatedByPolicy\":false}"

internal const val PARITY_TOURNAMENT_JSON = "{" +
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

/** Small JSON builder: preserves field order. */
internal fun obj(vararg fields: Pair<String, JsonValue>): JsonValue.Obj =
    JsonValue.Obj(LinkedHashMap<String, JsonValue>().apply { putAll(fields) })
