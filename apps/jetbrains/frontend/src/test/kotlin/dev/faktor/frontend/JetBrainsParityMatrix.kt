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
// against the pinned record for THIS PLATFORM and reports drift as a failure.
// Audit 28: the schema is `faktor-parity-visual-baselines/v3` with distinct
// `platforms.{linux,macos,windows}` records. A platform with no record is
// reported "not certified on platform <p>" and is NEVER compared against
// another platform's (or a canonical) digest; release claims require all
// three records (scripts/check-visual-platforms.mjs --release via
// certify.sh).
package dev.faktor.frontend

import dev.faktor.shared.JsonCodec
import dev.faktor.shared.JsonValue
import dev.faktor.shared.NativeBillingTaskUsage
import dev.faktor.shared.NativeCompletionContract
import dev.faktor.shared.NativeCreditBalance
import dev.faktor.shared.NativeMessage
import dev.faktor.shared.NativeRequests
import dev.faktor.shared.NativeUsageBuckets
import dev.faktor.shared.asciiLowerCase
import dev.faktor.shared.parseNativeAgents
import dev.faktor.shared.parseNativeBoardPage
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
import java.awt.Font
import java.awt.font.FontRenderContext
import java.awt.image.BufferedImage
import java.io.File
import java.math.BigInteger
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
    /** `passed` | `drifted` | `not_certified` (audit 28). */
    val status: String,
    val digest: String,
    val baseline: String?,
    /** The platform the render ran on (`linux` | `macos` | `windows` | `unknown`). */
    val platform: String,
    val detail: String
) {
    val passed: Boolean get() = status == "passed"
    val drifted: Boolean get() = status == "drifted"
    val notCertified: Boolean get() = status == "not_certified"
}

/** One platform's pinned visual baseline record. */
internal data class VisualPlatformRecord(
    val environment: String,
    val digests: Map<String, String>
)

/** The pinned visual baseline file: per-platform records only. There is no
 * canonical/shared digest pool: a platform without a record is NOT
 * CERTIFIED, never compared against another platform's digests. */
internal data class VisualBaseline(
    val schema: String,
    val requiredPlatforms: List<String>,
    val platforms: Map<String, VisualPlatformRecord>
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
    private var visualCoverage: Map<String, String> = emptyMap()

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
        val driver: ParityFakeDriver = { surface, observable, body ->
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

    /** Per-required-platform certification derived from the baseline file:
     * `certified` only when a distinct record with non-empty digests exists. */
    fun visualPlatformCoverage(): Map<String, String> = visualCoverage

    /** The platform this render runs on, mapped onto the release vocabulary. */
    private fun visualPlatform(): String {
        val os = asciiLowerCase(System.getProperty("os.name", "unknown"))
            .replace(Regex("[^a-z0-9]+"), "-")
        return when {
            os.contains("mac") -> "macos"
            os.contains("win") -> "windows"
            os.contains("linux") -> "linux"
            else -> "unknown"
        }
    }

    /**
     * Audit 28 policy core (pure, testable): a platform without a record in
     * the baseline file is `not_certified` and is never compared against any
     * other platform's digests; a missing panel inside a present record and
     * a digest mismatch are both `drifted`; a match is `passed`.
     */
    internal fun visualResultsFor(
        platform: String,
        baseline: VisualBaseline?,
        digests: Map<String, String>
    ): List<ParityVisualResult> {
        val record = baseline?.platforms?.get(platform)
        return digests.map { (panel, digest) ->
            when {
                record == null -> ParityVisualResult(
                    panel, "not_certified", digest, null, platform,
                    "not certified on platform $platform " +
                        "(no baseline record for this platform; the canonical pin is never inherited)"
                )
                !record.digests.containsKey(panel) -> ParityVisualResult(
                    panel, "drifted", digest, null, platform,
                    "panel $panel missing from the certified $platform baseline record"
                )
                record.digests.getValue(panel) != digest -> ParityVisualResult(
                    panel, "drifted", digest, record.digests.getValue(panel), platform,
                    "component-tree/state digest drifted from the certified $platform baseline"
                )
                else -> ParityVisualResult(
                    panel, "passed", digest, record.digests.getValue(panel), platform,
                    "digest matches the certified $platform baseline (${record.environment})"
                )
            }
        }
    }

    /** Release-claim coverage: a required platform is certified only by its
     * OWN non-empty record whose environment carries the RESOLVED font
     * fingerprint and names that platform. A legacy record without the
     * fingerprint (e.g. `mac-os-x-aarch64-jvm17`) is NOT certifiable
     * evidence and must read `not_certified` — release proof is red until a
     * real platform host accepts it (audit 23 / closure). */
    internal fun visualCoverageOf(baseline: VisualBaseline?): Map<String, String> {
        val out = LinkedHashMap<String, String>()
        for (platform in REQUIRED_VISUAL_PLATFORMS) {
            val record = baseline?.platforms?.get(platform)
            val certified = record != null &&
                record.digests.isNotEmpty() &&
                record.digests.values.all { it.isNotEmpty() } &&
                hasFontFingerprint(record.environment) &&
                environmentPlatform(record.environment) == platform
            out[platform] = if (certified) "certified" else "not_certified"
        }
        return out
    }

    /** The release platform an environment string names, or `unknown`. */
    private fun environmentPlatform(environment: String): String = when {
        environment.startsWith("linux-") -> "linux"
        environment.startsWith("mac-os") -> "macos"
        environment.startsWith("windows-") -> "windows"
        else -> "unknown"
    }

    /**
     * Adversarial self-test of the environment-specific policy (audit 28),
     * run by `JetBrainsParitySmoke`: a missing platform record is never
     * inherited, a mismatch drifts, a v2 file is unreadable, and unknown
     * platforms stay not certified.
     */
    internal fun visualCertificationPolicySelfTest() {
        val panels = listOf("task-tree", "settings")
        val linuxDigests = mapOf("task-tree" to "aa", "settings" to "bb")
        val macosDigests = mapOf("task-tree" to "cc", "settings" to "dd")
        fun baselineText(windows: Boolean): String {
            val out = StringBuilder()
            out.append("{\"schema\": \"$VISUAL_BASELINE_SCHEMA\",")
            out.append("\"requiredPlatforms\": [\"linux\",\"macos\",\"windows\"],")
            out.append("\"platforms\": {")
            out.append(
                "\"linux\": {\"environment\": \"linux-amd64-jvm17-fbeefbeef123\", \"digests\": {"
            )
            out.append("\"task-tree\": \"aa\", \"settings\": \"bb\"}},")
            out.append(
                "\"macos\": {\"environment\": \"mac-os-x-aarch64-jvm17-fcafecafe123\", \"digests\": {"
            )
            out.append("\"task-tree\": \"cc\", \"settings\": \"dd\"}}")
            if (windows) {
                out.append(
                    ",\"windows\": {\"environment\": \"windows-amd64-jvm17-fdeadbeef123\", \"digests\": {"
                )
                out.append("\"task-tree\": \"ee\", \"settings\": \"ff\"}}")
            }
            out.append("}}")
            return out.toString()
        }
        val complete = parseVisualBaseline(JsonCodec.parse(baselineText(windows = true)))
            ?: fail("self-test: complete v3 baseline must parse")
        val missingWindows = parseVisualBaseline(JsonCodec.parse(baselineText(windows = false)))
            ?: fail("self-test: v3 baseline without windows must parse")
        assertEquals(
            mapOf(
                "linux" to "certified",
                "macos" to "certified",
                "windows" to "certified"
            ),
            visualCoverageOf(complete),
            "self-test: complete coverage"
        )
        assertEquals(
            mapOf(
                "linux" to "certified",
                "macos" to "certified",
                "windows" to "not_certified"
            ),
            visualCoverageOf(missingWindows),
            "self-test: missing platform is not certified"
        )
        // A missing platform record must NOT inherit the linux digests.
        val windowsResults = visualResultsFor("windows", missingWindows, linuxDigests)
        assertTrue(
            windowsResults.all { it.notCertified && it.status != "passed" },
            "self-test: missing windows record must be not_certified"
        )
        // Drift against this platform's own record is a failure.
        val drifted = visualResultsFor(
            "linux",
            complete,
            mapOf("task-tree" to "aa", "settings" to "zz")
        )
        assertTrue(
            drifted.single { it.panel == "settings" }.drifted,
            "self-test: a digest mismatch must drift"
        )
        assertTrue(
            drifted.single { it.panel == "task-tree" }.passed,
            "self-test: a matching digest must pass"
        )
        // A v2 file is unreadable: no canonical inheritance.
        val v2 = JsonCodec.parse(
            "{\"schema\": \"faktor-parity-visual-baselines/v2\", \"panelDigests\": {\"task-tree\": \"aa\", \"settings\": \"bb\"}}"
        )
        assertTrue(
            parseVisualBaseline(v2) == null,
            "self-test: v2 canonical digests must not be readable as platform records"
        )
        val v2Results = visualResultsFor("linux", parseVisualBaseline(v2), linuxDigests)
        assertTrue(
            v2Results.all { it.notCertified },
            "self-test: a v2 baseline certifies no platform"
        )
        // Unknown platforms stay not certified even with full records.
        val unknown = visualResultsFor("solaris", complete, panels.associateWith { "aa" })
        assertTrue(
            unknown.all { it.notCertified },
            "self-test: unknown platforms are not certified"
        )
        assertTrue(
            visualCoverageOf(complete).values.all { it == "certified" },
            "self-test: complete coverage must be certified"
        )
        // A legacy record without the RESOLVED font fingerprint is not
        // certifiable evidence even with non-empty digests.
        val unresolvedText = baselineText(windows = true)
            .replace("-fbeefbeef123", "")
            .replace("-fcafecafe123", "")
            .replace("-fdeadbeef123", "")
        val unresolved = parseVisualBaseline(JsonCodec.parse(unresolvedText))
            ?: fail("self-test: unresolved-font baseline must parse")
        assertTrue(
            visualCoverageOf(unresolved).values.all { it == "not_certified" },
            "self-test: an unresolved-font record must not certify a platform"
        )
        // A record whose environment names another platform is never that
        // platform's evidence.
        val mislabeled = parseVisualBaseline(
            JsonCodec.parse(
                baselineText(windows = true)
                    .replace("mac-os-x-aarch64-jvm17-fcafecafe123", "linux-amd64-jvm17-fcafecafe123")
            )
        ) ?: fail("self-test: mislabeled baseline must parse")
        assertEquals(
            "not_certified",
            visualCoverageOf(mislabeled)["macos"],
            "self-test: a foreign environment must not certify macos"
        )
        // Font/environment attribution: a record whose environment carries a
        // fingerprint that does not match this host reports not_certified for
        // mismatching rows (never a false code drift), while a matching
        // digest under that environment still passes.
        val foreignEnvironment = baselineText(windows = true)
            .replace("linux-amd64-jvm17-fbeefbeef123", "linux-amd64-jvm17-fdeadbeef1234")
        val fingerprinted = parseVisualBaseline(JsonCodec.parse(foreignEnvironment))
            ?: fail("self-test: fingerprinted v3 baseline must parse")
        val environmentMismatch = applyVisualEnvironmentPolicy(
            "linux",
            fingerprinted,
            visualResultsFor(
                "linux",
                fingerprinted,
                mapOf("task-tree" to "aa", "settings" to "zz")
            )
        )
        assertTrue(
            environmentMismatch.single { it.panel == "settings" }.notCertified,
            "self-test: a mismatch on a foreign font environment must be not_certified"
        )
        assertTrue(
            environmentMismatch.single { it.panel == "task-tree" }.passed,
            "self-test: a matching digest stays passed even on a foreign environment"
        )
        // A legacy record without a fingerprint keeps the drift comparison.
        val legacyMismatch = applyVisualEnvironmentPolicy(
            "linux",
            unresolved,
            visualResultsFor(
                "linux",
                unresolved,
                mapOf("task-tree" to "aa", "settings" to "zz")
            )
        )
        assertTrue(
            legacyMismatch.single { it.panel == "settings" }.drifted,
            "self-test: a legacy record without a fingerprint keeps drift semantics"
        )
        assertTrue(
            fontFingerprint().length == 64,
            "self-test: the font fingerprint must be a full sha256 hex string"
        )
    }

    // ------------------------------------------------------------- rows

    private fun registerRows() {
        register(
            ParityRow(
                "task mode",
                canned = {
                    // Observable: the strict `POST …/task-runs` body carries
                    // goal + criteria + mutation_mode, and the ONE composer
                    // (Work) is the only place a completion contract is set.
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
                        assertTrue(
                            panel.tabTitles().containsAll(
                                listOf(
                                    "Work", "Inspect", "History",
                                    "Overview", "Plan", "Changes", "Verification", "Agents"
                                )
                            ),
                            panel.tabTitles().toString()
                        )
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
                    // The strict reply DTO carries the OWNING session id.
                    assertEquals(
                        "{\"session_id\":\"9\",\"permission_id\":\"7\",\"decision\":\"allow\"}",
                        NativeRequests.permissionReply("9", "7", "allow")
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
                    // The primary row is shell-like once the argv is observed.
                    panel.noteSpawned("5", "bash", listOf("-lc", "echo-parity"), "/tmp")
                    assertTrue(
                        panel.commandsText().contains("$ bash -lc echo-parity"),
                        panel.commandsText()
                    )
                    assertTrue(panel.commandsText().contains("(in /tmp)"), panel.commandsText())
                    panel.setOutput(parseNativeTerminalOutput("6", PARITY_TERMINAL_OUTPUT_JSON))
                    assertTrue(panel.outputText().contains("parity-output"), panel.outputText())
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
                    panel.setUnavailable("prior work read refused (status 503)")
                    assertEquals(false, panel.available())
                    panel.update(sessions, "7")
                    assertEquals(2, panel.count())
                    // Human rows: title, deterministic UTC time, outcome.
                    assertTrue(panel.label(0).contains("● Open now"), panel.label(0))
                    assertTrue(panel.label(0).contains("2023-11-14 22:13 UTC"), panel.label(0))
                    assertEquals(SemanticState.POSITIVE, panel.outcomeTone(0))
                    // Search filters the rows without re-reading the daemon.
                    panel.setFilterForTest("older")
                    assertEquals(1, panel.count())
                    assertTrue(panel.label(0).contains("✓ Completed"), panel.label(0))
                    panel.setFilterForTest("")
                    assertEquals(2, panel.count())
                    panel.select(1)
                    assertEquals("8", panel.selectedId())
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
                    // The daemon/stream machinery lives on Diagnostics.
                    val panel = DiagnosticsPanel()
                    panel.setConnection("connected: http://127.0.0.1:9", "open", 42L, "7")
                    assertEquals(true, panel.restartEnabled())
                    assertEquals(true, panel.reconnectEnabled())
                    assertTrue(panel.streamText().contains("cursor 42"), panel.streamText())
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
     * canonical component-tree/state digest and compares it against the
     * pinned baseline record FOR THIS PLATFORM (audit 28). A platform with
     * no record is reported `not_certified` and is NEVER compared against
     * another platform's digests; a mismatch against this platform's own
     * record is drift. `-Dfaktor.parity.writeBaselines=true` records this
     * platform's render.
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
        val platform = visualPlatform()
        val baselineFile = File(ParityPath.repoRoot(), PARITY_BASELINE_PATH)
        if (System.getProperty("faktor.parity.writeBaselines") == "true") {
            assertTrue(
                platform != "unknown",
                "cannot pin a visual baseline on an unknown platform"
            )
            writeVisualBaselines(baselineFile, platform, digests)
            visualCoverage = visualCoverageOf(readVisualBaseline(baselineFile))
            return digests.map { (name, digest) ->
                ParityVisualResult(
                    name, "passed", digest, digest, platform,
                    "baseline pinned from this render on $platform"
                )
            }
        }
        val baseline = readVisualBaseline(baselineFile)
        visualCoverage = visualCoverageOf(baseline)
        return applyVisualEnvironmentPolicy(
            platform,
            baseline,
            visualResultsFor(platform, baseline, digests)
        )
    }

    /**
     * A digest mismatch is attributable to code ONLY when the render ran in
     * the certified environment. When the record carries a font fingerprint
     * and this host's environment differs, mismatching rows are reported
     * `not_certified` with the typed environment reason rather than a false
     * `drifted` regression. A matching digest stays `passed` (a match under
     * another environment is stronger evidence), and a record without a
     * fingerprint (legacy) keeps the digest-only comparison.
     */
    internal fun applyVisualEnvironmentPolicy(
        platform: String,
        baseline: VisualBaseline?,
        results: List<ParityVisualResult>
    ): List<ParityVisualResult> {
        val record = baseline?.platforms?.get(platform) ?: return results
        val current = visualEnvironment()
        if (!hasFontFingerprint(record.environment) || record.environment == current) {
            return results
        }
        return results.map { result ->
            if (result.drifted) {
                ParityVisualResult(
                    result.panel,
                    "not_certified",
                    result.digest,
                    result.baseline,
                    platform,
                    "rendering environment differs from the certified record " +
                        "(record: ${record.environment}; this host: $current); " +
                        "the digest comparison is not attributable to code drift"
                )
            } else {
                result
            }
        }
    }

    /** True when an environment string carries a resolved-font fingerprint. */
    private fun hasFontFingerprint(environment: String): Boolean {
        val marker = environment.lastIndexOf("-f")
        if (marker < 0) return false
        val token = environment.substring(marker + 2)
        return token.length >= 8 && token.all { it in '0'..'9' || it in 'a'..'f' }
    }

    /**
     * Parses a v3 baseline: per-platform records only. A v2 file (canonical
     * `panelDigests` + `environmentDigests`) is NOT readable here — the
     * canonical pin must never be inherited as a platform record. Malformed
     * content returns null (which surfaces as `not_certified`, never a pass).
     */
    private fun parseVisualBaseline(json: JsonValue): VisualBaseline? {
        val root = json.view("baseline")
        val schema = root.field("schema").string()
        if (schema != VISUAL_BASELINE_SCHEMA) return null
        val required = root.field("requiredPlatforms").array().map { it.string() }
        val platformObj = root.field("platforms").value as? JsonValue.Obj ?: return null
        val platforms = LinkedHashMap<String, VisualPlatformRecord>()
        for ((platform, value) in platformObj.fields) {
            val record = value.view("platforms.$platform").objectValue()
            val environment = record.field("environment").string()
            val digestObj = record.field("digests").value as? JsonValue.Obj ?: return null
            val digests = LinkedHashMap<String, String>()
            for ((panel, digest) in digestObj.fields) {
                digests[panel] = digest.view("platforms.$platform.digests.$panel").string()
            }
            platforms[platform] = VisualPlatformRecord(environment, digests)
        }
        return VisualBaseline(schema, required, platforms)
    }

    private fun readVisualBaseline(file: File): VisualBaseline? {
        if (!file.isFile) return null
        return try {
            parseVisualBaseline(JsonCodec.parse(file.readText(Charsets.UTF_8)))
        } catch (e: Exception) {
            null
        }
    }

    /**
     * The stable fingerprint of the rendering host: OS + arch + JVM major +
     * resolved logical-font metrics (the component/state digest's bounds
     * derive from font metrics, so a record is only valid for the font
     * environment it was pinned in). A record's fingerprint lets a host with
     * different fonts report `not_certified` with a typed environment reason
     * instead of a false `drifted` code regression.
     */
    private fun visualEnvironment(): String {
        fun slug(raw: String): String =
            asciiLowerCase(raw).replace(Regex("[^a-z0-9]+"), "-").trim('-')
        val os = slug(System.getProperty("os.name", "unknown")).ifEmpty { "unknown" }
        val arch = slug(System.getProperty("os.arch", "unknown")).ifEmpty { "unknown" }
        val jvm = System.getProperty("java.version", "unknown")
            .split('.', '-', '+')
            .firstOrNull()
            ?.filter { it in '0'..'9' }
            .orEmpty()
            .ifEmpty { "unknown" }
        return "$os-$arch-jvm$jvm-f${fontFingerprint().take(12)}"
    }

    /**
     * Deterministic hash of the RESOLVED logical-font metrics (family, style,
     * a fixed string's advance width, line ascent/descent). Physical font
     * files are not readable through the AWT API, and names are not stable
     * identities, so the metrics are the portable identity: identical font
     * environments hash identically, a font-set or font-version change does
     * not.
     */
    internal fun fontFingerprint(): String {
        val sb = StringBuilder()
        val context = FontRenderContext(null, true, true)
        for (family in listOf(Font.DIALOG, Font.SANS_SERIF, Font.SERIF, Font.MONOSPACED)) {
            for (style in listOf(Font.PLAIN, Font.BOLD, Font.ITALIC, Font.BOLD or Font.ITALIC)) {
                val font = Font(family, style, 12)
                val sample = "Faktor parity 0123"
                val bounds = font.getStringBounds(sample, context)
                val metrics = font.getLineMetrics(sample, context)
                sb.append(family).append('|').append(style).append('|')
                    .append(font.family).append('|')
                    .append(bounds.width).append('|')
                    .append(metrics.ascent).append('|')
                    .append(metrics.descent).append('\n')
            }
        }
        return ParityPath.sha256Hex(sb.toString().toByteArray(Charsets.UTF_8))
    }

    /**
     * Records THIS platform's render as its own baseline record (audit 28),
     * preserving every other platform record verbatim. A v2 file is refused:
     * its canonical `panelDigests` must be migrated explicitly to the
     * platform that actually produced them (never inherited silently).
     */
    private fun writeVisualBaselines(file: File, platform: String, digests: Map<String, String>) {
        val expected = File(ParityPath.repoRoot(), PARITY_BASELINE_PATH)
        assertEquals(
            expected.canonicalPath, file.canonicalPath,
            "visual baselines must be written to the pinned in-tree path"
        )
        assertTrue(
            REQUIRED_VISUAL_PLATFORMS.contains(platform),
            "platform $platform is not in the release vocabulary"
        )
        val existing = if (file.isFile) {
            try {
                JsonCodec.parse(file.readText(Charsets.UTF_8))
            } catch (e: Exception) {
                null
            }
        } else {
            null
        }
        if (existing != null) {
            assertTrue(
                parseVisualBaseline(existing) != null,
                "existing visual baseline is not $VISUAL_BASELINE_SCHEMA; migrate the v2 " +
                    "canonical digest to a platform record first (never inherit it)"
            )
        }
        file.parentFile.mkdirs()
        val platforms = LinkedHashMap<String, VisualPlatformRecord>()
        readVisualBaseline(file)?.let { platforms.putAll(it.platforms) }
        platforms[platform] = VisualPlatformRecord(visualEnvironment(), LinkedHashMap(digests))
        val out = StringBuilder()
        out.append("{\n")
        out.append("  \"schema\": \"$VISUAL_BASELINE_SCHEMA\",\n")
        out.append("  \"method\": ")
        JsonCodec.writeString(out, VISUAL_METHOD)
        out.append(",\n")
        // One-time re-pin provenance, preserved verbatim across re-pins: the
        // Windows record must be produced by a Windows render (its own
        // `windows-...-f<hex>` environment fingerprint) via the owned lane.
        out.append("  \"rePin\": {\n")
        out.append("    \"windows\": ")
        JsonCodec.writeString(out, VISUAL_WINDOWS_RE_PIN)
        out.append(",\n")
        out.append("    \"linux\": ")
        JsonCodec.writeString(out, VISUAL_LINUX_RE_PIN)
        out.append(",\n")
        out.append("    \"fonts\": ")
        JsonCodec.writeString(out, VISUAL_FONT_ENVIRONMENT)
        out.append("\n  },\n")
        out.append("  \"requiredPlatforms\": [")
        for ((index, required) in REQUIRED_VISUAL_PLATFORMS.withIndex()) {
            JsonCodec.writeString(out, required)
            out.append(if (index == REQUIRED_VISUAL_PLATFORMS.size - 1) "],\n" else ", ")
        }
        out.append("  \"platforms\": {\n")
        val keys = platforms.keys.sorted()
        for ((index, key) in keys.withIndex()) {
            out.append("    ")
            JsonCodec.writeString(out, key)
            out.append(": {\n")
            out.append("      \"environment\": ")
            JsonCodec.writeString(out, platforms.getValue(key).environment)
            out.append(",\n")
            out.append("      \"digests\": {\n")
            val entries = platforms.getValue(key).digests
            val entryKeys = entries.keys.toList()
            for ((e, panel) in entryKeys.withIndex()) {
                out.append("        ")
                JsonCodec.writeString(out, panel)
                out.append(": ")
                JsonCodec.writeString(out, entries.getValue(panel))
                out.append(if (e == entryKeys.size - 1) "\n" else ",\n")
            }
            out.append("      }\n")
            out.append("    }")
            out.append(if (index == keys.size - 1) "\n" else ",\n")
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
        val visualStatus = when {
            visual.isEmpty() -> "unavailable"
            visual.any { it.drifted } -> "failed"
            visual.any { it.notCertified } -> "not_certified"
            else -> "passed"
        }
        val visualComplete = visualStatus == "passed"
        val visualPlatform = if (visual.isEmpty()) "unknown" else visual.first().platform
        val coverage = if (visualCoverage.isEmpty()) {
            visualCoverageOf(null)
        } else {
            visualCoverage
        }
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
                "status" to JsonValue.Str(visualStatus),
                "passed" to JsonValue.Int64(visualPassed.toLong()),
                "total" to JsonValue.Int64(visual.size.toLong()),
                "method" to JsonValue.Str(VISUAL_METHOD),
                "baseline" to JsonValue.Str(PARITY_BASELINE_PATH),
                "platform" to JsonValue.Str(visualPlatform),
                "platforms" to obj(
                    *REQUIRED_VISUAL_PLATFORMS.map { platform ->
                        platform to JsonValue.Str(coverage[platform] ?: "not_certified")
                    }.toTypedArray()
                ),
                "panels" to JsonValue.Arr(
                    visual.map { result ->
                        obj(
                            "panel" to JsonValue.Str(result.panel),
                            "status" to JsonValue.Str(result.status),
                            "platform" to JsonValue.Str(result.platform),
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
                "visual $visualPassed/${visual.size} panels ($visualStatus on $visualPlatform), " +
                "platforms " +
                REQUIRED_VISUAL_PLATFORMS.joinToString(",") {
                    "$it=" + (coverage[it] ?: "not_certified")
                } +
                ", artifact $PARITY_PATH @ $commit"
        )
    }
}

/** The fake-daemon row driver: the suite submits one check per surface.
 * A function type (not `fun interface`) so the pinned kotlinc 1.3.31 in the
 * hermetic CI image can compile it. */
internal typealias ParityFakeDriver = (surface: String, observable: String, body: () -> Unit) -> Unit

/**
 * Submits the fake-daemon observable one surface row names. The suite owns
 * the daemon; the row speaks only in (surface, observable key).
 */
internal fun fakeDaemonCheck(surface: String, observable: String) {
    val driver = ParityMatrixRegistry.current
        ?: fail("the fake-daemon suite is not running (row $surface)")
    val body = ParityMatrixRegistry.observables[observable]
        ?: fail("no fake-daemon observable named '$observable' for row $surface")
    driver(surface, observable, body)
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
            // Swing validates a container repeatedly: a width-aware wrapped
            // label changes its preferred height during the first pass, and
            // the real window settles on the re-validated layout. Reproduce
            // the settled passes instead of digesting a stale single pass.
            for (pass in 0 until 3) {
                panel.doLayout()
                layoutChildren(panel)
            }
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

/** One-time per-platform re-pin provenance written into the baseline file
 * (and preserved across re-pins). The Windows record may ONLY come from the
 * Windows-owned lane; a platform is never pinned from another host. */
internal const val VISUAL_WINDOWS_RE_PIN =
    "powershell -NoProfile -ExecutionPolicy Bypass -File scripts/windows-visual-baseline.ps1 -WriteBaselines (on the Windows agent); commit target/certification/visual-baselines-windows.json into this file's windows record"
internal const val VISUAL_LINUX_RE_PIN =
    "bash apps/jetbrains/compile-and-smoke.sh --write-baselines (or ./gradlew :frontend:smoke -PwriteBaselines=true) on the Linux host, under the pinned core-fonts fontconfig"
internal const val VISUAL_FONT_ENVIRONMENT =
    "apps/jetbrains/frontend/src/test/resources/parity/fonts/core-fonts.conf (FONTCONFIG_FILE); digest bounds derive from resolved font metrics"

/** Baseline schema v3: distinct per-platform records, no inherited pin. */
internal const val VISUAL_BASELINE_SCHEMA = "faktor-parity-visual-baselines/v3"

/** Release claims require a distinct certified record for every platform. */
internal val REQUIRED_VISUAL_PLATFORMS = listOf("linux", "macos", "windows")

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

/** The Overview view: the plan-derived run summary at a glance. */
internal fun cannedOverviewPanel(): OverviewPanel {
    val panel = OverviewPanel()
    panel.updateModel(criterionProofModel())
    return panel
}

/** The Changes view: changed files plus their contextual evidence refs. */
internal fun cannedChangesPanel(): ChangesPanel {
    val panel = ChangesPanel()
    panel.update(
        listOf("apps/jetbrains/frontend/FaktorChatPanel.kt", "docs/ui-terminology.md"),
        criterionProofModel().evidence
    )
    return panel
}

/** The Verification view: criteria verdicts, checks and reviewer. */
internal fun cannedVerificationPanel(): VerificationPanel {
    val panel = VerificationPanel()
    panel.updateModel(criterionProofModel())
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

/** A populated board with a real older-page cursor (host-matrix render). */
internal fun cannedBoardPanel(): BoardPanel {
    val panel = BoardPanel()
    panel.setBoard(parseNativeBoardPage(PARITY_BOARD_POSTS_JSON))
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
    return panel
}

/** A populated agent roster with the served ownership/model/budget fields. */
internal fun cannedAgentsPanel(): AgentsPanel {
    val panel = AgentsPanel()
    panel.update(parseNativeAgents(PARITY_AGENTS_JSON))
    return panel
}

/** A representative diagnostics readout (muted key/value rows + recovery). */
internal fun cannedDiagnosticsPanel(): DiagnosticsPanel {
    val panel = DiagnosticsPanel()
    panel.setConnection("attached port 9 version fake-1", "open", 42L, "7")
    panel.setDaemon("daemon: attached port 9 version fake-1")
    panel.setStream("stream: open at cursor 42")
    panel.setState("state: awaiting_permission (blocked on shell approval)")
    panel.setModel("model: alpha/m (reasoning)")
    panel.setTool("active tool: bash [running]")
    panel.setQueued("queued: 2")
    panel.setUsage("usage: sessions=2 tokens=4200 taskCostMicro=1234567")
    panel.setVerification("verification: owed=1 failed=1")
    panel.setFiles("files changed: 3")
    panel.setIndex("index: partial generation 7 (capped fingerprint round)")
    return panel
}

/** A populated usage page: active subscription, quotas, credits and tasks. */
internal fun cannedUsagePanel(): UsagePanel {
    val panel = UsagePanel()
    val totals = NativeUsageBuckets(
        inputTokens = 1200,
        outputTokens = 800,
        cacheReadTokens = 150,
        cacheWriteTokens = 50,
        reasoningTokens = 25,
        providerCostMicro = BigInteger("4200000"),
        managedCostMicro = BigInteger("1234567"),
        byokCostMicro = BigInteger("90000"),
        events = 17,
        correctedEvents = 1
    )
    panel.setModel(
        UsagePanelModel(
            state = "ok",
            reason = null,
            organization = "acme",
            planId = "pro",
            planFound = true,
            subscription = "active",
            subscriptionStatus = "active",
            subscriptionExpiresMs = 1_900_000_000_000L,
            subscriptionActive = true,
            totals = totals,
            tasks = listOf(NativeBillingTaskUsage(taskId = 3, runId = "run-9", totals = totals)),
            credits = NativeCreditBalance(
                grantedMicro = BigInteger("10000000"),
                consumedMicro = BigInteger("250000"),
                refundedMicro = BigInteger.ZERO,
                heldMicro = BigInteger("1000"),
                pendingConsumes = 1
            ),
            quotas = listOf(
                UsageQuotaRow(
                    limit = USAGE_LIMIT_MAX_TOKENS,
                    value = BigInteger("100000"),
                    observed = BigInteger("2200"),
                    exceeded = false,
                    kind = "ceiling",
                    reason = null
                ),
                UsageQuotaRow(
                    limit = USAGE_LIMIT_MANAGED_SPEND,
                    value = BigInteger("1000000"),
                    observed = BigInteger("1234567"),
                    exceeded = true,
                    kind = "ceiling",
                    reason = null
                )
            ),
            inFlight = emptyList(),
            cursor = null,
            nextCursor = "cursor-2",
            itemCount = 50,
            hasPrev = false,
            canGrantCredits = true,
            grantDisabledReason = null
        )
    )
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
    "\"state\":\"ready\",\"created_ms\":1700000000000}," +
    "{\"id\":\"8\",\"title\":\"older\",\"provider\":\"beta\",\"model\":\"n\"," +
    "\"state\":\"ended\",\"created_ms\":1700000000000}]}"

/**
 * The advertised attachment contract of the parity fake daemon: tight enough
 * that the oversize-refusal row exercises a real bound (a 5 KiB PDF is
 * refused against the 4 KiB per-document bound) while the accept rows stay
 * small and fast. Every value is daemon-ADVERTISED; the client never mirrors.
 */
internal const val PARITY_ATTACHMENT_LIMITS_JSON = "{" +
    "\"maxUploadBytes\":65536,\"maxRequestBytes\":131072,\"maxAttachmentBytes\":262144," +
    "\"image\":{\"mimes\":[{\"mime\":\"image/png\",\"maxBytes\":65536}," +
    "{\"mime\":\"image/jpeg\",\"maxBytes\":65536}],\"maxRequestBytes\":131072}," +
    "\"document\":{\"capable\":true,\"mimes\":[{\"mime\":\"application/pdf\",\"maxBytes\":4096}," +
    "{\"mime\":\"text/plain\",\"maxBytes\":4096}],\"maxRequestBytes\":8192}}"

internal const val PARITY_MODELS_JSON = "[" +
    "{\"provider\":\"alpha\",\"model\":\"m\",\"context\":1000,\"maxOutput\":100," +
    "\"tools\":true,\"parallelTools\":false,\"reasoning\":true,\"thinking\":false," +
    "\"vision\":true,\"structuredOutput\":false,\"embeddings\":false," +
    "\"streaming\":true,\"source\":\"conservativeDefault\"," +
    "\"documentCapable\":true,\"attachmentLimits\":" + PARITY_ATTACHMENT_LIMITS_JSON + "}," +
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

/** A populated board page with `has_more` for the host-matrix board render. */
internal const val PARITY_BOARD_POSTS_JSON = "{" +
    "\"board_id\":7,\"revision\":3,\"posts\":[" +
    "{\"id\":3,\"board_id\":7,\"author_child\":8,\"author_session\":8," +
    "\"subject\":\"handoff\",\"body\":\"main step ready\",\"refs\":[\"evidence:41\"]," +
    "\"revision\":3,\"created_ms\":1700}," +
    "{\"id\":2,\"board_id\":7,\"author_child\":null,\"author_session\":7," +
    "\"subject\":\"root note\",\"body\":\"no children yet\",\"refs\":[]," +
    "\"revision\":2,\"created_ms\":1600}" +
    "],\"next_before_revision\":2,\"has_more\":true}"

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
