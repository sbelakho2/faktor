// Native Swing tool-window panel for the Faktor bridge. Every interaction
// routes to a native endpoint through FaktorFrontendService:
//
//   chat input  -> POST /native/session/{id}/prompt (abort -> POST /native/session/{id}/abort)
//   task goal   -> POST /native/session/{id}/task-runs (cancel -> .../cancel)
//   agents      -> GET /native/agents + pause/resume/cancel/retry/steer/model/budget
//   status      -> GET /session/{id}/projection, /native/usage,
//                  /native/session/{id}/usage, /native/session/{id}/verification
//   evidence    -> GET /native/evidence/{id} + POST /native/evidence/{id}/retrieve
//   streaming   -> SSE /native/session/{id}/events with cursor resume
//
// Swing/JCEF decision: JCEF hosting needs the IntelliJ Platform/JCEF runtime
// which the repository's offline build does not carry, so the panel is a
// native Swing component (zero third-party deps, compiles with plain
// kotlinc like the rest of the split-mode tree) that a future
// ToolWindowFactory can embed directly.
package dev.faktor.frontend

import dev.faktor.backend.LiveChunkFrame
import dev.faktor.backend.LiveOutputMessage
import dev.faktor.backend.LiveSnapshot
import dev.faktor.backend.NativeSseEvent
import dev.faktor.backend.ProtocolBlocked
import dev.faktor.shared.JsonValue
import dev.faktor.shared.MicroMoney
import dev.faktor.shared.NativeAgent
import dev.faktor.shared.NativeApiException
import dev.faktor.shared.NativeCompletionContract
import dev.faktor.shared.NativeContractRows
import dev.faktor.shared.NativeModelInfo
import dev.faktor.shared.NativePermissionEntry
import dev.faktor.shared.NativePermissionReplyRefusal
import dev.faktor.shared.indexCoverageLabel
import dev.faktor.shared.NativeTaskRun
import dev.faktor.shared.NativeTournament
import dev.faktor.shared.NativeTaskView
import dev.faktor.shared.NativeSessionSummary
import dev.faktor.shared.toJson
import java.math.BigInteger
import java.nio.file.Path
import java.util.UUID
import java.awt.BorderLayout
import java.awt.CardLayout
import java.awt.FlowLayout
import java.awt.GridLayout
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference
import javax.swing.BorderFactory
import javax.swing.ButtonGroup
import javax.swing.DefaultComboBoxModel
import javax.swing.JButton
import javax.swing.JComponent
import javax.swing.JComboBox
import javax.swing.JMenuItem
import javax.swing.JPopupMenu
import javax.swing.JFrame
import javax.swing.JLabel
import javax.swing.JMenu
import javax.swing.JOptionPane
import javax.swing.JPanel
import javax.swing.JTabbedPane
import javax.swing.JTextArea
import javax.swing.JTextField
import javax.swing.JToggleButton
import javax.swing.SwingUtilities
import javax.swing.WindowConstants

private const val MAX_TRANSCRIPT_LINES = 4000
private const val MAX_TRANSCRIPT_CHARS = 400_000
private const val MAX_EVIDENCE_PREVIEW_CHARS = 4000
private const val MAX_CHILD_USAGE_FETCHES = 12

/** Bounded wait for the sign-out revoke; it runs off the EDT. */
private const val SIGN_OUT_REVOKE_TIMEOUT_MS = 5_000L

/** Routine provider chunks never reach the transcript; durable milestones do. */
private const val MAX_NOTABLE_EVENTS_PER_FLUSH = 8

/** One stream frame plus the session that produced it (stale batches drop). */
private data class StreamDelta(val sessionId: String?, val event: NativeSseEvent)

/**
 * Live model output is host-batched per 16–33ms window: every whole-code-
 * point delta is applied to the part-keyed overlay and ONE EDT flush renders
 * the window. A burst can never cause a restart/render per token.
 */
private const val LIVE_FLUSH_INTERVAL_MS = 16

/** Bound of live frames applied in ONE EDT flush (beyond it: flush now). */
private const val LIVE_FLUSH_MAX_BATCH = 128

/** Bounded cooldown of the authoritative resync fetch (no resync storm). */
private const val LIVE_RESYNC_COOLDOWN_MS = 250L

/**
 * Bounded subscribe-before-submit wait: the worker waits at most this long
 * for the live handshake snapshot BEFORE the prompt POST. A daemon without
 * live output releases it immediately (typed `unavailable`).
 */
private const val LIVE_HANDSHAKE_WAIT_MS = 1_000L

/**
 * One completion-contract FINISH LEVEL of the four-level selector. The wire
 * mapping is EXACT and hierarchical: every later level explicitly includes
 * the earlier actions, exactly like the VS Code finish-level radio group.
 */
internal enum class FinishLevel(
    private val includeCommit: Boolean,
    private val includePush: Boolean,
    private val includePr: Boolean,
    private val labelText: String
) {
    LOCAL(false, false, false, "Local — leave changes here (nothing requested)"),
    COMMIT(true, false, false, "Commit — includes local changes, commits when verified"),
    PUSH(true, true, false, "Push — includes commit, pushes when verified"),
    PR(true, true, true, "PR — includes push, opens a pull request when verified");

    /**
     * The exact [NativeCompletionContract] of this level. LOCAL requests no
     * conditional step at all: the daemon's default path (no contract sent),
     * mirrored by [completionContractFromControls] returning null.
     */
    fun contract(): NativeCompletionContract? =
        if (this == LOCAL) null else NativeCompletionContract(includeCommit, includePush, includePr)

    /** The visible label (the earlier actions are named explicitly). */
    fun label(): String = labelText
}

/** One Inspect detail option: the selector label + its single surface. */
private data class InspectDetail(val label: String, val view: java.awt.Component)

private fun inspectDetail(label: String, view: java.awt.Component) = InspectDetail(label, view)

class FaktorChatPanel(
    private val service: FaktorFrontendService,
    /** The IDE credential store for the control-plane token (PasswordSafe in
     * production); null only in display tests with no credential wiring. */
    private val controlPlaneCredentials: ControlPlaneCredentialStore? = null,
    /** The NON-secret (endpoint, organization) coordinates. */
    private val controlPlaneScopeStore: ControlPlaneScopeStore? = null,
    /** The NON-secret auth-session id the sign-out revoke route names. */
    private val controlPlaneSessionStore: ControlPlaneSessionStore? = null,
    /**
     * The session workspace root: the IntelliJ host passes `Project.basePath`
     * and the standalone launcher its working directory. The SAME root rides
     * `POST /native/session` (so the daemon resolves workspace-relative
     * attachment paths against it) and attachment relativization; the daemon's
     * session projection exposes no workspace path, so the project base path
     * is the client authority. Null = no root: ordinary attachments are
     * refused typed instead of being sent as absolute paths.
     */
    private val workspaceRoot: Path? = null,
    /**
     * The shared application-settings controller (I08): the Settings utility
     * loads/persists the run defaults through it, exactly like the platform
     * `FaktorConfigurable`. Null keeps the selection in-panel (display tests,
     * standalone embedding).
     */
    private val settingsController: FaktorSettingsController? = null,
    /**
     * The I03 editor host for the immutable diff review (the platform
     * `ChangeReviewAdapter` in production). Null keeps the pure surface but
     * refuses an open with a typed reason instead of silently doing nothing.
     */
    private val changeReviewHost: ChangeReviewHost? = null,
    /** Revision 8 host-local MRU/context surfaces (never daemon state). */
    private val edition8Favorites: MutableSet<String> = linkedSetOf(),
    private val edition8Drafts: MutableList<String> = ArrayList(),
    /**
     * Revision 8 editor host seam. Production passes
     * `IntelliJHostEditor`; the split-mode smoke and display tests leave it
     * null so the panel never links against platform classes it does not own.
     */
    private val edition8HostEditor: Edition8HostEditor? = null,
    /** Bounded remote-revoke budget used by the background sign-out worker. */
    private val signOutRevokeTimeoutMs: Long = SIGN_OUT_REVOKE_TIMEOUT_MS
) : JPanel(BorderLayout()), FaktorFrontendService.Listener {

    private val worker = Executors.newSingleThreadExecutor { runnable ->
        val thread = Thread(runnable, "faktor-ui")
        thread.isDaemon = true
        thread
    }

    private val refreshQueued = AtomicBoolean(false)

    /** True when a stream flush arrived while a slow refresh was in flight. */
    private val refreshDirty = AtomicBoolean(false)

    /** EDT/work counters (test + harness observable; never rendered). */
    private val uiMetrics = UiWorkMetrics()

    /**
     * Urgent control-plane actions (abort/cancel/stop/permission reply) must
     * NOT serialize behind slow refresh/read work: they get their own
     * single-thread authority (bounded, daemon thread, reaped on shutdown).
     */
    private val controlWorker = Executors.newSingleThreadExecutor { runnable ->
        val thread = Thread(runnable, "faktor-control")
        thread.isDaemon = true
        thread
    }

    // Single-flight gate of the task-start surface: set BEFORE the start job
    // is enqueued, so repeated clicks can never accumulate duplicate start
    // jobs in the worker queue. Cleared only by an explicit result.
    private val startInFlight = AtomicBoolean(false)

    // Sign-out is a remote call; it never runs on the EDT. The gate makes
    // repeated clicks a no-op while the worker owns the operation.
    private val signOutInFlight = AtomicBoolean(false)

    // The immutable submission retried after a TRANSPORT failure: the same
    // submission id and the exact captured draft, reused by the next logical
    // click (a changed draft is a new logical start and gets a new id).
    @Volatile
    private var pendingTaskSubmission: TaskStartSubmission? = null

    /**
     * Host-journey seam: observes every logical send from the Work composer
     * (submission id + captured goal) as the real IDE journey dispatches it.
     */
    internal var taskStartObserver: ((String, String) -> Unit)? = null

    /**
     * The component transcript: stable You/Faktor identities, real styled
     * Markdown blocks (headings/lists/emphasis/code/links/tables), a Copy
     * action on every code block, and turn-grouped assistant parts. Row
     * cells keep their Swing component identity across updates.
     */
    private val transcriptView = TranscriptView()

    /** The transcript viewport + the follow/detach scroll policy. */
    private val transcriptScroll = insetScroll(transcriptView)

    private val transcriptPin = ScrollPinController(transcriptScroll)

    /** One batched render pass per flush; keyed rows update in place. */
    private val transcriptSink = TranscriptSink(
        transcriptView,
        transcriptPin,
        uiMetrics,
        MAX_TRANSCRIPT_CHARS,
        MAX_TRANSCRIPT_LINES
    )

    /** Per-session teardown (queued deltas, offsets, transient view state). */
    private var sessionScope = SessionUiScope(uiMetrics)

    /**
     * The delta pump: the stream thread only enqueues; the EDT sees one
     * bounded flush per interval instead of one invokeLater per journal
     * frame. Order is preserved and each flush drains at most
     * [UI_FLUSH_MAX_BATCH] frames, so a sustained producer cannot starve the
     * event queue.
     */
    private val streamPump = EdtFlushPump<StreamDelta>(
        "stream",
        UI_FLUSH_INTERVAL_MS,
        UI_FLUSH_MAX_BATCH,
        uiMetrics,
        { delta: StreamDelta -> delta.event.dataBytes }
    ) { batch -> applyStreamBatch(batch) }

    /**
     * Slow-surface refresh cadence: message/board/tree/agent reads are
     * expensive (several HTTP reads + JSON parses), so they run at most once
     * per [UI_REFRESH_INTERVAL_MS] while the stream is hot.
     */
    private val refreshDelayTimer = javax.swing.Timer(UI_REFRESH_INTERVAL_MS) {
        refreshIfDirty()
    }.apply {
        isRepeats = false
        isCoalesce = true
    }

    /** The last applied durable sequence of the current session (replay dedupe). */
    private var lastAppliedEventSeq = 0L

    /** True once the panel's own children exist (guards construction-time updateUI). */
    private var uiInitialized = false

    /** The ONE composer input: every request to Faktor starts here. */
    private val input = JTextArea(3, 60)

    /** Single-flight for the ordinary prompt path: one logical prompt at a time. */
    private val promptInFlight = AtomicBoolean(false)

    /**
     * The immutable logical prompt held across a RETRYABLE failure: the same
     * draft retries under the same submission id (the daemon replays the
     * receipt), a typed refusal or a changed draft starts a fresh one.
     */
    @Volatile
    private var pendingPromptSubmission: PromptSubmission? = null

    // Service lifecycle stays under More ▾ → Diagnostics (taxonomy law:
    // infrastructure is not default-surface vocabulary); the buttons remain
    // the single dispatch/enable authority for the menu items.
    private val startButton = secondaryButton("Start service")

    private val stopButton = secondaryButton("Stop service")

    private val serviceStartItem = JMenuItem("Start service")

    private val serviceStopItem = JMenuItem("Stop service")

    private val newSessionButton = secondaryButton("New session")

    private val refreshButton = secondaryButton("Refresh")

    // ONE submit action for the ONE composer. Its label follows the intent;
    // there is no second Ask/Run-task button beside it.
    private val submitButton = primaryButton(ComposerIntent.default().submitLabel())

    private val abortButton = secondaryButton("Abort")

    private val criteriaField = JTextField(24)

    // The "Acceptance & contract" rows: one entry per line, bounded at
    // capture time exactly like the daemon (<=64 lines, <=512 UTF-8 bytes).
    // Blank lines are dropped; a violation refuses the start loudly rather
    // than silently discarding an authored constraint.
    private val constraintField = JTextArea(2, 24)

    private val invariantField = JTextArea(2, 24)

    private val riskField = JTextArea(2, 24)

    private val obligationField = JTextArea(2, 24)

    private val attachments = AttachmentsPanel(workspaceRoot)

    // The per-run budget of the options sheet (Task mode only): empty uses
    // the machine/run defaults; a served value must parse exactly like the
    // agent budget controls (positive decimal integer, no larger than the
    // money domain), else the start refuses loudly.
    private val budgetTokensField = JTextField(24)

    private val budgetCostField = JTextField(24)

    // Audit 29: bounded local pending-upload state. A failed start keeps the
    // already-uploaded ids; the retry resolves them first and uploads only
    // the absent attachments (parity with the VS Code pending envelope).
    private val pendingAttachmentRetry = PendingAttachmentRetry()

    // The composer's run options: ONE disclosure next to the composer opens
    // the bounded, scrollable Task options sheet (contract + finish level +
    // per-run budget + attachments). The submitted contract is tracked so the
    // tree can report durable step provenance.
    internal val completionLevels: List<FaktorToggleButton> = FinishLevel.values().map { level ->
        FaktorToggleButton(level.label()).apply {
            toolTipText = level.label()
        }
    }

    private val completionGroup = ButtonGroup()

    private var submittedCompletion: NativeCompletionContract? = null

    // The ONE disclosure for the Task-only options (criteria, finish level,
    // contract rows, per-run budget, attachments). It is only reachable in
    // Task intent; Ask never shows or sends any of it.
    private val taskOptionsButton = FaktorToggleButton("Task options")

    // Task/Ask mode (design addition, named for the two paths the ONE
    // composer already has): Task sends the contract/criteria/attachments/
    // budget with the goal; Ask sends the plain prompt and NEVER attaches a
    // completion contract or finish actions.
    private val taskModeButton = FaktorToggleButton("Task")

    private val askModeButton = FaktorToggleButton("Ask")

    private val modeGroup = ButtonGroup()

    private val runSummaryArea = JTextArea(3, 24)

    // The compact sticky Work status row: state, run, tokens and approvals
    // only. Run details (goal, phase, candidate boundary) live in Inspect.
    private val workStateChip = statusChip("Ready", SemanticState.DIM)

    // The cause-specific supporting copy and its ONE action (Settings/Retry/
    // Resume/Diagnostics/Reply). Hidden while the state has no action.
    private val workStateCopy = wrappedMutedLabel("Ask a question, or choose Task for an isolated run.")

    private val workStateAction = secondaryButton("Wait")

    private val workRunLabel = mutedLabel("No active run")

    private val workTokensLabel = mutedLabel("Tokens —")

    private val workApprovalLabel = mutedLabel("No approvals")

    private val workDetailsButton = secondaryButton("Details")

    private val workStopButton = secondaryButton("Stop")

    // T06: the composer's ready/empty state — the exact design label+copy
    // render while the composer holds no draft; the existing Run task control
    // is the action. Typing hides the row (the state IS the empty composer).
    private val composerReadyLabel = sectionHeader(RunStateCopy.READY_LABEL, muted = true)

    private val composerHint = wrappedMutedLabel(RunStateCopy.READY_COPY)

    private val composerReadyRow = wrapRow(composerReadyLabel, composerHint)

    // The newest served task view + task-run state tag of the current run:
    // the strip's retained authority across client-side start transitions
    // (T07/T08 change without a fresh served snapshot).
    private var currentRunView: NativeTaskView? = null

    private var currentRunStateTag: String? = null

    /** The moved run-details surface (Inspect › Overview › Run details). */
    private var runDetailsPanel: JComponent? = null

    // The served proof task state of the current run (`GET
    // /native/tasks/{id}/proof` -> `task.state`): the ONLY source of the T12
    // checking state, which the session/task-run view collapses into
    // `Running`. Null until a successful proof read serves it; a refused read
    // clears it (a stale checking state is never kept).
    private var currentProofTaskState: String? = null

    // Cause-specific UX state signals. They are observed facts (not a second
    // projection): the last failure class, the daemon lifecycle and the live
    // stream state. The pure mapper in UxStateModel.kt owns the precedence.
    private var daemonLifecycle = "stopped"

    /** True once this panel observed the daemon running (crash signal gate). */
    private var daemonObservedRunning = false

    private var lastStreamStatus = "off"

    /** True once the live stream opened (a retry after that is a real drop). */
    private var streamObservedOpen = false

    private var streamPaused = false

    private var lastFailure: UxFailure? = null

    private enum class UxFailure { CREDENTIAL, PROVIDER_TRANSIENT, NETWORK, PRE_RUN, UNRECOVERABLE }

    // T22: a requested PR finish step blocked by the missing canonical SCM
    // integration surfaces the EXACT copy with one action pointing at the
    // same Diagnostics setup guidance card. Hidden otherwise; PR success is
    // never inferred.
    private val prSetupHeadline = sectionHeader("Pull request integration unavailable")

    private val prSetupCopy =
        wrappedMutedLabel("Configure the native SCM integration; no PR success is inferred.")

    private val prSetupButton = secondaryButton("Open setup guidance")

    private val prSetupBody = pageColumn(gap = Spacing.XS, padding = 0)

    private val inspectRunsButton = secondaryButton("Inspect")

    private val sessionChooserModel = DefaultComboBoxModel<NativeSessionSummary>()

    private val sessionChooser = JComboBox(sessionChooserModel)

    private val openSessionButton = secondaryButton("Open")

    private val runsModel = DefaultComboBoxModel<NativeTaskRun>()

    private val runsCombo = JComboBox(runsModel)

    private val cancelRunButton = secondaryButton("Cancel run")

    /**
     * The agent-control input resolver, invoked ON the EDT by
     * [agentInputOnEdt] (the production default is the modal JOptionPane
     * prompt; tests inject a recording / cancelling provider).
     */
    internal var agentInputProvider: ((java.awt.Component, String, String?) -> String?)? = null

    /**
     * Test seam observing the worker-side dispatch of one agent control: the
     * callback runs on the `faktor-ui` worker immediately before the control
     * call, so a smoke can prove the dialog never ran there.
     */
    internal var agentControlDispatchObserver: ((String) -> Unit)? = null

    private val treePanel = TaskTreePanel()

    private val blockersPanel = BlockersPanel()

    private val tournamentPanel = TournamentPanel()

    private val navigator = EvidenceNavigatorPanel()

    private val boardPanel = BoardPanel()

    /**
     * The per-request approval reply gate shared by BOTH decision surfaces
     * (the Work attention card above the composer and the Inspect -> Agents
     * -> Approvals list): one reply per request id, whoever clicked first.
     */
    private val approvalGate = ApprovalReplyGate()

    private val permissionsPanel = PermissionsPanel(approvalGate)

    /**
     * The bounded Work attention card: the live pending request set directly
     * above the composer, hidden while nothing is pending. Shares the card
     * renderer and the reply gate with [permissionsPanel].
     */
    private val workApprovalCard = WorkApprovalCard(approvalGate)

    /**
     * Test/embedding seam: observes the once-per-request approval
     * announcements. The production announcement rides the card's accessible
     * visible-data channel; no approval path ever requests focus.
     */
    internal var approvalAnnounceObserver: ((String) -> Unit)? = null

    private val terminalPanel = TerminalPanel()

    private val settingsPanel = SettingsPanel(settingsController)

    private val historyPanel = HistoryPanel()

    /** Edition 5 context inspector and compaction review (session-scoped). */
    private val edition5ContextPanel = Edition5ContextPanel(null) { service.currentSessionId() }

    private val edition5CompactionPanel = Edition5CompactionPanel(null) { service.currentSessionId() }

    /** Edition 5 checkpoint timeline / restore / fork (session-scoped). */
    private val edition5CheckpointPanel = Edition5CheckpointPanel(null) { service.currentSessionId() }

    private val edition5ModelLabel = mutedLabel("Model: defaults from Settings")

    private val edition5ModelButton = secondaryButton("Model…")

    private val edition5ContextLabel = mutedLabel("Context: Unavailable (contextUsage = null)")

    private val edition5ContextButton = secondaryButton("Context")

    private val edition5Attention = statusChip("No pending attention", SemanticState.DIM)

    private var edition5AttachedFor: String? = null

    /** The I07 Outcomes surface (lazy-read on first open), beside the list. */
    private val outcomesPanel = OutcomesPanel()

    private val historyTabs = FaktorTabbedPane()

    private val usagePanel = UsagePanel()

    private val diagnosticsPanel = DiagnosticsPanel()

    private val agentsPanel = AgentsPanel()

    private val overviewPanel = OverviewPanel()

    private val changesPanel = ChangesPanel()

    private val verificationPanel = VerificationPanel()

    /** The agent roster with its contextual approvals and coordination. */
    private val agentsTabs = FaktorTabbedPane()

    private val workPlane = JPanel(BorderLayout())

    /** The bounded PasswordSafe row watch (no platform change event exists). */
    private val controlPlaneWatcher: ControlPlaneCredentialWatcher?

    // The organization-wide usage cursor stack: Previous pages are replayed
    // exactly from their recorded cursors, never recomputed.
    private var billingCursor: String? = null

    // The Account surface is LAZY: no identity/entitlement/billing read is
    // issued by a run refresh or by opening Work/Inspect. The first explicit
    // Account open (or its Refresh) performs the read; while no control-plane
    // credential exists it says "Account not connected" without a request.
    private var accountLoaded = false

    private var accountReadAttempts = 0

    /** True while a usage page request is in flight: rapid Next/Previous
     * clicks must not push the same cursor twice against a stale model. */
    private val usagePagingInFlight = java.util.concurrent.atomic.AtomicBoolean(false)

    private val billingPrevCursors = ArrayList<String>()

    // The outcomes ledger read: lazy on first Outcomes open + explicit
    // Refresh; the paging cursor stack replays exact older-page cursors.
    private var outcomesCursor: dev.faktor.shared.ProtocolOutcomeCursor? = null

    private val outcomesPrevCursors =
        ArrayList<dev.faktor.shared.ProtocolOutcomeCursor?>()

    private val outcomesPagingInFlight = AtomicBoolean(false)

    private var outcomesLoaded = false

    /** The inspector root: 3 top-level cluster destinations (Work/Inspect/History). */
    private val tabs = FaktorTabbedPane()

    /** Inspect: Overview / Plan / Changes / Verification / Agents. */
    private val inspectTabs = FaktorTabbedPane()

    // The single selected detail surface per Inspect tab (I01): the selector
    // row swaps ONE visible detail; the selection is keyed by tab title so a
    // refresh/snapshot never resets it. Declared before init: buildLayout()
    // registers every selector through it.
    private val inspectSelection = HashMap<String, String>()

    private val contentHost = JPanel(BorderLayout())

    private val utilityHeader = WrappedLabel("")

    /** The wrapping utilities toolbar row (built by [buildLayout]). */
    private lateinit var utilityToolbar: JPanel

    /** Developer-only registry item (More -> Diagnostics -> Developer). */
    private var developerRegistryItem: JMenuItem? = null

    private var developerRegistryPath: List<String> = emptyList()

    private var utilityShown = false

    /**
     * The durable transcript rows: id-keyed with ordered indices and a
     * content-revision guard (see [TranscriptModel]). The durable seq
     * decides ORDERING only — it never gates whether an existing row's
     * content may update (the P0 "permanently incomplete reply" defect).
     */
    private val transcriptModel = TranscriptModel()

    /**
     * The live model-output pump: the stream thread only offers parsed
     * chunks; the EDT applies and renders ONE coalesced window. Bounded by
     * [UI_MAX_QUEUED] frames and [UI_MAX_QUEUED_BYTES] payload bytes.
     */
    private val livePump = EdtFlushPump<LiveChunkFrame>(
        "live",
        LIVE_FLUSH_INTERVAL_MS,
        LIVE_FLUSH_MAX_BATCH,
        uiMetrics,
        { frame: LiveChunkFrame -> frame.content.length }
    ) { batch -> applyLiveBatch(batch) }

    /**
     * The ONE turn reducer (idle → submitting → queued/starting → streaming/
     * tool-running/awaiting-user → completed | interrupted | failed). HTTP
     * acceptance is admission only; completion comes from durable authority.
     */
    private var turnLifecycle: TurnLifecycle = TurnLifecycle.empty()

    /** The last (display phase, stop op) rendered (dedupe). */
    private var lastPostedTurnKey: String? = null

    /** Single-flight + cooldown of the authoritative live resync fetch. */
    private val liveResyncInFlight = AtomicBoolean(false)

    private var liveResyncLastAt = 0L

    private var currentTree: TaskTreeModel? = null

    private var currentAgents: List<NativeAgent> = emptyList()

    private var trackedTournamentId: String? = null

    private var trackedTournament: NativeTournament? = null

    private var pendingPermissions: List<NativePermissionEntry> = emptyList()



    init {
        service.setListener(this)
        // Per-session teardown: queued deltas from the old session must never
        // render into the new one.
        sessionScope.onDispose { streamPump.discardPending() }
        sessionScope.onDispose { livePump.discardPending() }
        // A live offset/revision gap requests the AUTHORITATIVE part snapshot
        // (single-flight, bounded cooldown); a gap never fabricates text.
        transcriptModel.setLiveGapListener { partId -> requestLiveResync("gap $partId") }
        // Clipboard/paste staging notices (refusals included) surface in the
        // transcript exactly once, on the EDT.
        attachments.onNotice = { message -> onEdt { appendSystem(message) } }
        buildLayout()
        wireActions()
        updateWorkStatus()
        setControlsEnabled(false)
        val scope = controlPlaneScopeStore?.read()
        if (scope != null) {
            settingsPanel.setControlPlaneScope(scope)
        }
        settingsPanel.setControlPlaneSession(controlPlaneSessionStore?.read())
        settingsPanel.setControlPlaneStatus(controlPlaneStatusText(controlPlaneCredentials?.resolve(scope)))
        val credentials = controlPlaneCredentials
        val scopeStore = controlPlaneScopeStore
        controlPlaneWatcher = if (credentials != null && scopeStore != null) {
            // NOT started here: the panel host starts it with the daemon, and
            // tests drive pollOnce() synchronously (no background timing).
            ControlPlaneCredentialWatcher(
                store = credentials,
                scopeStore = scopeStore,
                onTokenChanged = { token -> applyWatchedControlPlaneToken(token) }
            )
        } else {
            null
        }
        uiInitialized = true
    }

    /**
     * A live IDE theme/HiDPI font change updates the components this panel
     * owns (the platform recreates other UIs). Re-wrapped labels invalidate
     * their HTML on [setFont] independently.
     */
    override fun updateUI() {
        super.updateUI()
        if (!uiInitialized) return
        transcriptView.applyFont(uiPanelFont().deriveFont(13f))
        input.font = uiPanelFont()
        runSummaryArea.font = uiPanelFont()
        transcriptSink.trimIfNeeded()
        revalidate()
        repaint()
    }

    // ------------------------------------------------------------------ UI

    /**
     * The real host adapter for the Revision 8 screens. Its callbacks reuse
     * the panel's live service, session and task scope; absent scope is a
     * typed refusal in the screen, never an invented value.
     */
    internal fun edition8Host(): Edition8Host = Edition8Host(
        client = { FaktorConnectionRegistry.client() },
        sessionId = { service.currentSessionId() },
        taskId = {
            runCatching { service.taskRuns().firstOrNull()?.taskId?.toString() }.getOrNull()
        },
        vault = null,
        settings = settingsController,
        startService = {
            val health = service.start()
            "started ok=${health.ok} version=${health.version}"
        },
        restartService = {
            val health = service.restart()
            "restarted ok=${health.ok} version=${health.version}"
        },
        inlineCodeAction = {
            edition8HostEditor?.inlineCodeAction()
                ?: "contract-required: no IDE editor host is attached; no inline code action was claimed"
        },
        hostAction = { id ->
            when (id) {
                "M-03" -> {
                    val model = "current"
                    val added = if (edition8Favorites.add(model)) {
                        "added to"
                    } else {
                        edition8Favorites.remove(model)
                        "removed from"
                    }
                    "$model $added local favorites. This is host MRU state; no daemon write is claimed."
                }
                "C-04" -> edition8HostEditor?.captureSelection()?.let { captured ->
                    if (edition8Drafts.size >= 32) edition8Drafts.removeAt(0)
                    edition8Drafts.add(captured)
                    "Captured the current editor selection at its source version."
                } ?: "No active editor selection exists; nothing was attached."
                "S-05" -> edition8HostEditor?.insertSlashCommand()
                    ?: "No active editor exists; no command text was inserted."
                "X-10" -> edition8HostEditor?.openProblems()
                    ?: "No project is open; no diagnostics surface exists."
                "Q-11" -> "Sandbox setup is host-owned; the daemon policy remains fail-closed. Open Settings > Tools > Faktor to review setup guidance."
                "D-08" -> "Display focus is host-local; choose Compact, Balanced or Verbose in the IDE appearance settings."
                "D-09" -> "Theme, zoom and density are host-owned; Faktor never overrides them. Open Settings > Appearance."
                "D-10" -> "Send behavior is a host keybinding; the default remains Ctrl/Cmd+Enter."
                "D-11" -> "Keymaps and command discovery are host-owned; open Settings > Keymap to inspect them."
                "D-14" -> "Accessibility and reduced-motion preferences are read from the host; open Settings > Appearance > Accessibility."
                else -> "contract-required: no host action is defined for $id"
            }
        },
        confirmTrust = { path ->
            SwingUtilities.invokeLater { startButton.doClick() }
            "start requested for $path — awaiting daemon readiness"
        },
        startTask = { goal, options ->
            val criteria = options["V-07"]?.takeIf { it.isNotBlank() }
                ?.split('\n')?.filter { it.isNotBlank() }
            val model = options["M-01"]?.takeIf { it.isNotBlank() }
            val started = service.startTaskRun(goal, criteria = criteria, model = model)
            "task-run ${started.runId} task ${started.taskId} state ${started.state}"
        },
        stopScope = { scope ->
            when (scope) {
                "run" -> {
                    val run = service.taskRuns().firstOrNull()
                    if (run == null) "no owned run to stop — nothing was cancelled"
                    else {
                        val ack = service.cancelTaskRun(run.runId)
                        "cancel ${if (ack.cancelled) "acknowledged" else "refused"} for ${ack.runId}"
                    }
                }
                else -> "scope $scope is separate: no implicit stop was issued"
            }
        },
        openApprovals = {
            SwingUtilities.invokeLater {
                selectCluster("Inspect")
                for (i in 0 until inspectTabs.tabCount) {
                    if (inspectTabs.getTitleAt(i) == "Agents") inspectTabs.selectedIndex = i
                }
                for (i in 0 until agentsTabs.tabCount) {
                    if (agentsTabs.getTitleAt(i) == "Approvals") agentsTabs.selectedIndex = i
                }
            }
            "opened the approval cards; no decision was sent"
        },
        replyApproval = { id, session, decision ->
            val ack = service.replyPermission(session, id, decision)
            "permission $id $decision acknowledged=${ack.ok}"
        },
        openDiff = {
            SwingUtilities.invokeLater {
                selectCluster("Inspect")
                inspectTabs.selectedIndex = 3
            }
            "opened native diff destination (Inspect › Changes)"
        },
        openTerminal = {
            SwingUtilities.invokeLater {
                selectCluster("Inspect")
                inspectTabs.selectedIndex = 0
                selectInspectDetail("Overview", "Terminal")
            }
            "opened the owned console surface"
        },
        navigate = { screenId ->
            SwingUtilities.invokeLater { Edition8ScreenBrowser(edition8Host()).show(this, screenId) }
        },
        credentials = controlPlaneCredentials
    )


    private fun buildLayout() {
        transcriptView.applyFont(uiPanelFont().deriveFont(13f))

        input.lineWrap = true
        input.wrapStyleWord = true
        input.font = uiPanelFont()
        input.a11y(
            "Message to Faktor",
            "Describe a goal or ask a question. Ctrl+Enter runs the task; Escape closes Task options."
        )
        // Ctrl+Enter in the composer is the keyboard twin of Run task.
        input.inputMap.put(
            javax.swing.KeyStroke.getKeyStroke(
                java.awt.event.KeyEvent.VK_ENTER,
                java.awt.event.InputEvent.CTRL_DOWN_MASK
            ),
            "faktor.runTask"
        )
        input.actionMap.put("faktor.runTask", object : javax.swing.AbstractAction() {
            override fun actionPerformed(event: java.awt.event.ActionEvent?) {
                startTaskFromControls()
            }
        })
        // macOS hosts expect the platform menu shortcut (Cmd+Enter). Bind it
        // beside Ctrl+Enter; on non-macOS the mask equals Ctrl and the entry
        // is already present.
        val menuMask = if (java.awt.GraphicsEnvironment.isHeadless()) {
            java.awt.event.InputEvent.CTRL_DOWN_MASK
        } else {
            runCatching { java.awt.Toolkit.getDefaultToolkit().menuShortcutKeyMaskEx }
                .getOrDefault(java.awt.event.InputEvent.CTRL_DOWN_MASK)
        }
        if (menuMask != java.awt.event.InputEvent.CTRL_DOWN_MASK) {
            input.inputMap.put(
                javax.swing.KeyStroke.getKeyStroke(java.awt.event.KeyEvent.VK_ENTER, menuMask),
                "faktor.runTask"
            )
        }
        // Escape closes the Task options sheet (returning focus to the exact
        // trigger); it NEVER cancels a run.
        input.inputMap.put(
            javax.swing.KeyStroke.getKeyStroke(java.awt.event.KeyEvent.VK_ESCAPE, 0),
            "faktor.closeOptions"
        )
        input.actionMap.put("faktor.closeOptions", object : javax.swing.AbstractAction() {
            override fun actionPerformed(event: java.awt.event.ActionEvent?) {
                if (taskOptionsSheetOpen()) closeTaskOptionsSheet()
            }
        })
        // The same Escape contract works from anywhere in the panel (a list,
        // a field, a card): close the options/utility surface and return to
        // the surface the operator came from. Focused components that bind
        // Escape themselves keep their own action.
        getInputMap(JComponent.WHEN_ANCESTOR_OF_FOCUSED_COMPONENT).put(
            javax.swing.KeyStroke.getKeyStroke(java.awt.event.KeyEvent.VK_ESCAPE, 0),
            "faktor.globalClose"
        )
        actionMap.put("faktor.globalClose", object : javax.swing.AbstractAction() {
            override fun actionPerformed(event: java.awt.event.ActionEvent?) {
                when {
                    taskOptionsSheetOpen() -> closeTaskOptionsSheet()
                    utilityShown -> hideUtility()
                }
            }
        })

        // The utilities toolbar WRAPS at narrow host widths (ActionRowPanel
        // pattern) instead of revealing overflow through a horizontal
        // scrollbar: at 240px every primary control (New session, Refresh,
        // the session chooser) stays visible, and the secondary utilities
        // live under More. No horizontal-scroll fallback exists.
        // The screen registry is a developer/Diagnostics surface only; it is
        // not part of the normal Work/Inspect/History navigation.
        val toolbar = wrapRow(FaktorMark(), newSessionButton, refreshButton)
        toolbar.isOpaque = true
        toolbar.background = panelSurface()
        toolbar.border = BorderFactory.createCompoundBorder(
            BorderFactory.createMatteBorder(0, 0, 1, 0, FaktorTheme.separator()),
            BorderFactory.createEmptyBorder(Spacing.XS, Spacing.M, Spacing.XS, Spacing.M)
        )
        // The session chooser sits between Refresh and More: a bounded
        // listing of the durable sessions (title + served state; a missing
        // value stays missing) whose Open reuses the exact HistoryPanel open
        // path. It refreshes with every snapshot.
        sessionChooser.toolTipText = "Open a durable session"
        sessionChooser.a11y(
            "Durable sessions",
            "Choose a prior conversation, then press Enter to open it."
        )
        sessionChooser.maximumRowCount = 12
        sessionChooser.minimumSize = java.awt.Dimension(140, sessionChooser.minimumSize.height)
        sessionChooser.renderer = SessionChooserRenderer()
        installActivateKey(sessionChooser, "faktor.openSession") { openSelectedSession() }
        toolbar.add(sessionChooser)
        openSessionButton.toolTipText = "Open the selected durable session"
        openSessionButton.a11y("Open selected session", "Opens the chosen durable conversation.")
        openSessionButton.addActionListener { openSelectedSession() }
        toolbar.add(openSessionButton)
        val moreButton = secondaryButton("More ▾")
        val moreMenu = JPopupMenu()
        val settingsItem = JMenuItem("Settings")
        settingsItem.addActionListener { showUtility("Settings", settingsPanel) }
        moreMenu.add(settingsItem)
        val accountItem = JMenuItem("Account")
        accountItem.addActionListener { openAccount() }
        moreMenu.add(accountItem)
        // Diagnostics is a normal destination; the screen registry lives one
        // level deeper under Diagnostics -> Developer and is never a normal
        // Work/Inspect/History destination.
        val diagnosticsMenu = JMenu("Diagnostics")
        val openDiagnosticsItem = JMenuItem("Open diagnostics")
        openDiagnosticsItem.addActionListener { showUtility("Diagnostics", diagnosticsPanel) }
        diagnosticsMenu.add(openDiagnosticsItem)
        val developerMenu = JMenu("Developer")
        val screenRegistryItem = JMenuItem("Screen registry…")
        screenRegistryItem.addActionListener {
            Edition8ScreenBrowser(edition8Host()).show(this)
        }
        developerRegistryItem = screenRegistryItem
        developerRegistryPath =
            listOf(diagnosticsMenu.text, developerMenu.text, screenRegistryItem.text)
        developerMenu.add(screenRegistryItem)
        diagnosticsMenu.add(developerMenu)
        moreMenu.add(diagnosticsMenu)
        moreMenu.addSeparator()
        serviceStartItem.addActionListener { startButton.doClick() }
        serviceStopItem.addActionListener { stopButton.doClick() }
        moreMenu.add(serviceStartItem)
        moreMenu.add(serviceStopItem)
        moreButton.addActionListener { moreMenu.show(moreButton, 0, moreButton.height) }
        toolbar.add(moreButton)
        utilityToolbar = toolbar

        buildWorkPlane()
        inspectTabs.removeAll()
        inspectTabs.addTab(
            "Overview",
            selectorTab(
                "Overview",
                listOf(
                    inspectDetail("Overview", overviewPanel),
                    inspectDetail("Run details", runDetailsPanel ?: JPanel()),
                    inspectDetail("Blockers", blockersPanel),
                    inspectDetail("Terminal", terminalPanel)
                )
            )
        )
        inspectTabs.addTab(
            "Plan",
            selectorTab(
                "Plan",
                listOf(
                    inspectDetail("Plan", treePanel),
                    inspectDetail("Compare approaches", tournamentPanel)
                )
            )
        )
        inspectTabs.addTab(
            "Context",
            selectorTab(
                "Context",
                listOf(
                    inspectDetail("Manifest", edition5ContextPanel),
                    inspectDetail("Compaction", edition5CompactionPanel)
                )
            )
        )
        inspectTabs.addTab("Changes", changesPanel)
        inspectTabs.addTab(
            "Verification",
            selectorTab(
                "Verification",
                listOf(
                    inspectDetail("Verification", verificationPanel),
                    inspectDetail("Evidence", navigator)
                )
            )
        )
        agentsTabs.removeAll()
        agentsTabs.addTab("Roster", agentsPanel)
        agentsTabs.addTab("Approvals", permissionsPanel)
        agentsTabs.addTab("Coordination", boardPanel)
        inspectTabs.addTab("Agents", agentsTabs)

        val tabs = this.tabs
        tabs.removeAll()
        tabs.addTab("Work", workPlane)
        tabs.addTab("Inspect", inspectTabs)
        // The History cluster keeps the conversations list intact and adds
        // the I07 Outcomes surface beside it; the ledger read is lazy (first
        // open) and failure-isolated from the conversation listing.
        historyTabs.removeAll()
        historyTabs.addTab("Conversations", historyPanel)
        historyTabs.addTab("Checkpoints", edition5CheckpointPanel)
        historyTabs.addTab("Outcomes", outcomesPanel)
        historyTabs.addChangeListener {
            ensureOutcomesLoaded()
            ensureEdition5SessionPanelsLoaded()
        }
        inspectTabs.addChangeListener { ensureEdition5SessionPanelsLoaded() }
        tabs.addChangeListener { ensureEdition5SessionPanelsLoaded() }
        tabs.addTab("History", historyTabs)

        add(toolbar, BorderLayout.NORTH)
        add(contentHost, BorderLayout.CENTER)
        contentHost.removeAll()
        contentHost.add(tabs, BorderLayout.CENTER)
    }

    /**
     * The Work surface: conversation (or the Task options sheet) + the run
     * summary strip + ONE composer. The composer carries the goal and the
     * Task/Ask mode; the bounded options sheet carries the acceptance
     * criteria, the finish level, the engineering-contract rows, the per-run
     * budget and the attachments.
     */
    private fun buildWorkPlane() {
        // The Work status row is the whole persistent chrome above the
        // conversation: state, the active run, exact tokens and pending
        // approvals. Everything else (model, context, goal, plan, candidate)
        // stays in Task options, Inspect or Settings.
        workStateChip.a11y("Run state", "The served state of the current run.")
        workRunLabel.a11y("Active run", "The phase or identity of the current run.")
        workTokensLabel.a11y("Token usage", "Exact spent and limit tokens served for this run.")
        workApprovalLabel.a11y("Pending approvals", "The number of requests waiting for a decision.")
        workDetailsButton.toolTipText = "Open the run details in Inspect"
        workDetailsButton.a11y("Open run details", "Opens the full run summary in Inspect.")
        workDetailsButton.addActionListener { inspectCurrentRun() }
        workStopButton.toolTipText = "Stop the live turn, or cancel the selected task run"
        workStopButton.a11y(
            "Stop the current run",
            "Requests cancellation of the live turn (the accepted operation) or the selected task run."
        )
        workStopButton.addActionListener {
            // The reducer names the accepted operation; only a task-run
            // surface without a live turn falls back to the run cancel.
            if (turnStopOpId(turnLifecycle) != null) stopActiveTurn() else cancelRunButton.doClick()
        }
        workStateCopy.a11y("Run state detail", "The cause-specific detail of the current run state.")
        workStateAction.addActionListener { performUxAction(currentUxState()) }
        val statusBody = pageColumn(gap = Spacing.XS, padding = 0)
        statusBody.add(wrapRow(workStateChip, workRunLabel, workTokensLabel, workApprovalLabel))
        statusBody.add(workStateCopy)
        statusBody.add(actionRow(workStateAction, workDetailsButton, workStopButton))
        statusBody.background = panelSurface()
        statusBody.border = BorderFactory.createEmptyBorder(Spacing.XS, Spacing.M, Spacing.XS, Spacing.M)
        workPlane.add(statusBody, BorderLayout.NORTH)

        val transcriptBody = JPanel(BorderLayout())
        transcriptBody.isOpaque = false
        transcriptBody.add(transcriptScroll, BorderLayout.CENTER)
        val conversation = card("Conversation", transcriptBody)
        // The center host swaps ONE visible surface: the conversation or the
        // bounded Task options sheet (never a blocking modal dialog).
        val centerHost = JPanel(CardLayout())
        centerHost.isOpaque = false
        centerHost.add(conversation, "conversation")
        centerHost.add(taskOptionsSheet(), "options")
        (centerHost.layout as CardLayout).show(centerHost, "conversation")
        optionsSheetHost = centerHost
        workPlane.add(centerHost, BorderLayout.CENTER)

        val south = pageColumn(gap = Spacing.S, padding = 0)
        south.border = BorderFactory.createEmptyBorder(Spacing.S, Spacing.M, Spacing.M, Spacing.M)

        // The run summary strip: the served goal/phase/candidate boundary of
        // the current run, its state and named steps, with Inspect (the
        // Overview tab) and the scoped Cancel run action. Missing values stay
        // missing; nothing is invented.
        runSummaryArea.isEditable = false
        runSummaryArea.lineWrap = true
        runSummaryArea.wrapStyleWord = true
        runSummaryArea.font = uiPanelFont()
        runSummaryArea.a11y("Current run summary", "Served run state, phase, steps and candidate boundary.")
        runsCombo.a11y("Task runs", "Choose the task run whose summary is shown.")
        val runsBody = pageColumn(gap = Spacing.XS, padding = 0)
        runsBody.add(runsCombo)
        runsBody.add(insetScroll(runSummaryArea))
        // T22: the blocked-PR surface (exact copy + one action pointing at
        // the Diagnostics setup guidance). Hidden until the daemon serves
        // the typed SCM-not-configured blocker on the requested PR step.
        prSetupBody.add(prSetupHeadline)
        prSetupBody.add(prSetupCopy)
        prSetupButton.addActionListener {
            showUtility("Diagnostics", diagnosticsPanel)
            diagnosticsPanel.revealSetupGuidance()
        }
        prSetupBody.add(actionRow(prSetupButton))
        prSetupBody.isVisible = false
        runsBody.add(prSetupBody)
        inspectRunsButton.toolTipText = "Open the Overview detail of the current run"
        inspectRunsButton.a11y("Inspect current run", "Opens the Overview detail of the current run.")
        inspectRunsButton.addActionListener { inspectCurrentRun() }
        cancelRunButton.toolTipText = "Cancel the selected task run"
        cancelRunButton.a11y("Cancel selected run", "Cancels the selected task run.")
        runsBody.add(actionRow(inspectRunsButton, cancelRunButton))
        // Run details are an Inspect destination, never a persistent board
        // between the conversation and the composer.
        runDetailsPanel = card("Run details", runsBody)

        // The bounded Work attention card sits ABOVE the composer (hidden
        // while nothing is pending): the live pending request set with its
        // per-request Allow/Deny/Details, sharing the Approvals list renderer
        // and the per-id reply gate.
        south.add(workApprovalCard)

        // The composer: ONE input, ONE Ask|Task intent control, ONE submit
        // action. Task options live behind the single disclosure and are
        // reachable only in Task intent. Ask is the default and uses the
        // real plain-prompt path (no contract, criteria, budget, attachments
        // or task-run request).
        taskOptionsButton.addActionListener { toggleTaskOptionsSheet() }
        modeGroup.add(taskModeButton)
        modeGroup.add(askModeButton)
        askModeButton.isSelected = ComposerIntent.default() == ComposerIntent.ASK
        taskModeButton.isSelected = ComposerIntent.default() == ComposerIntent.TASK
        taskModeButton.toolTipText =
            "Task: Run task sends the goal with criteria, contract, finish actions and attachments"
        askModeButton.toolTipText =
            "Ask: sends the plain prompt with no contract or task run"
        taskModeButton.a11y("Task intent", "Run task sends the goal with its Task options.")
        askModeButton.a11y("Ask intent", "Sends the plain prompt without a contract or task run.")
        submitButton.a11y("Send", "Sends the composer with the selected intent.")
        abortButton.a11y("Abort", "Requests abort of the active local operation.")
        askModeButton.addActionListener { applyIntentPresentation() }
        taskModeButton.addActionListener { applyIntentPresentation() }
        val modeRow = JPanel(FlowLayout(FlowLayout.LEFT, Spacing.XS, 0))
        modeRow.isOpaque = false
        modeRow.add(sectionHeader("Intent", muted = true))
        modeRow.add(askModeButton)
        modeRow.add(taskModeButton)
        applyIntentPresentation()
        val actions = actionRow(submitButton, abortButton, taskOptionsButton)
        val composerBody = pageColumn(gap = Spacing.S, padding = 0)
        composerBody.add(focusAccentScroll(input))
        // T06: the ready guidance appears exactly while the composer is
        // empty; the listener tracks the real draft (including the clears a
        // successful send performs), never a mirrored copy of it.
        input.document.addDocumentListener(object : javax.swing.event.DocumentListener {
            override fun insertUpdate(event: javax.swing.event.DocumentEvent?) {
                updateComposerReady()
            }

            override fun removeUpdate(event: javax.swing.event.DocumentEvent?) {
                updateComposerReady()
            }

            override fun changedUpdate(event: javax.swing.event.DocumentEvent?) {
                // Attribute-only change: the draft text (and the state) is unchanged.
            }
        })
        composerBody.add(composerReadyRow)
        updateComposerReady()
        // Model, context and attention stay one quiet composer row: they are
        // configuration beside the draft, not a second transcript header.
        edition5ModelButton.a11y("Choose model", "Opens the native model picker over the served catalog.")
        edition5ModelButton.addActionListener { openEdition5ModelPicker() }
        edition5ContextButton.a11y("Open context inspector", "Shows capacity, sources and compaction.")
        edition5ContextButton.addActionListener {
            selectCluster("Inspect")
            selectInspectDetail("Context", "Manifest")
        }
        composerBody.add(
            wrapRow(
                edition5ModelLabel, edition5ModelButton,
                edition5ContextLabel, edition5ContextButton,
                edition5Attention
            )
        )
        composerBody.add(modeRow)
        composerBody.add(actions)
        south.add(card("Ask Faktor", composerBody))
        workPlane.add(south, BorderLayout.SOUTH)
        updateWorkStatus()
    }

    /** T06: the ready row shows exactly while the composer holds no draft. */
    private fun updateComposerReady() {
        composerReadyRow.isVisible = input.text.trim().isEmpty()
        composerReadyRow.revalidate()
        composerReadyRow.repaint()
    }

    /**
     * The bounded Task options sheet: a header with an explicit Back plus a
     * scrollable body ([pageScroll] tracks the 240px viewport width), so no
     * blocking modal dialog is ever required and every field stays reachable.
     * The composer (and the draft inside it) stays below the sheet.
     */
    private fun taskOptionsSheet(): JPanel {
        val sheet = JPanel(BorderLayout())
        optionsSheetPanel = sheet
        sheet.background = panelSurface()
        val backButton = secondaryButton("Back")
        backButton.toolTipText = "Return to the conversation (Escape closes the sheet)"
        backButton.addActionListener { closeTaskOptionsSheet() }
        taskOptionsBackButton = backButton
        val headerRow = JPanel(BorderLayout(Spacing.S, 0))
        headerRow.isOpaque = true
        headerRow.background = panelSurface()
        headerRow.border = BorderFactory.createCompoundBorder(
            BorderFactory.createMatteBorder(0, 0, 1, 0, FaktorTheme.separator()),
            BorderFactory.createEmptyBorder(Spacing.S, Spacing.M, Spacing.S, Spacing.M)
        )
        val backRow = JPanel(FlowLayout(FlowLayout.LEFT, Spacing.S, 0))
        backRow.isOpaque = false
        backRow.add(backButton)
        headerRow.add(backRow, BorderLayout.WEST)
        headerRow.add(sectionHeader("Task options"), BorderLayout.CENTER)
        sheet.add(headerRow, BorderLayout.NORTH)
        sheet.add(pageScroll(taskOptionsBody()), BorderLayout.CENTER)
        val escape = javax.swing.KeyStroke.getKeyStroke(
            java.awt.event.KeyEvent.VK_ESCAPE,
            0
        )
        sheet.getInputMap(JComponent.WHEN_ANCESTOR_OF_FOCUSED_COMPONENT).put(
            escape, "faktor.closeOptions"
        )
        sheet.actionMap.put(
            "faktor.closeOptions",
            object : javax.swing.AbstractAction() {
                override fun actionPerformed(event: java.awt.event.ActionEvent?) {
                    closeTaskOptionsSheet()
                }
            }
        )
        return sheet
    }

    /**
     * The one options body (reused by the sheet): acceptance criteria, the
     * four-level finish selector, the contract rows, the per-run budget and
     * the attachments.
     */
    private fun taskOptionsBody(): JPanel {
        criteriaField.toolTipText = "Optional: acceptance criteria, comma separated"
        criteriaField.a11y("Acceptance criteria", "Comma-separated acceptance criteria for this task.")
        val finishBox = JPanel(GridLayout(FinishLevel.values().size, 1, Spacing.XS, Spacing.XS))
        finishBox.isOpaque = false
        for (button in completionLevels) {
            completionGroup.add(button)
            button.a11y(button.text, "Finish level for this task: ${button.text}")
            finishBox.add(button)
        }
        completionLevels.first().isSelected = true
        budgetTokensField.toolTipText =
            "max_tokens for this run: a positive integer; empty uses the run default"
        budgetCostField.toolTipText =
            "max_cost_micro for this run: a positive decimal integer; empty uses the run default"
        budgetTokensField.a11y("Max tokens", "Per-run max_tokens; empty uses the run default.")
        budgetCostField.a11y("Max cost micro", "Per-run max_cost_micro; empty uses the run default.")
        constraintField.a11y("Constraints", "One engineering constraint per line.")
        invariantField.a11y("Invariants", "One engineering invariant per line.")
        riskField.a11y("Risks", "One engineering risk per line.")
        obligationField.a11y("Proof obligations", "One proof obligation per line.")
        val budgetGrid = FormGrid()
        budgetGrid.row("max tokens", budgetTokensField)
        budgetGrid.row("max cost (microUSD)", budgetCostField)
        val optionsBody = pageColumn(gap = Spacing.S, padding = Spacing.S)
        optionsBody.add(FormGrid().row("Acceptance criteria (comma separated)", criteriaField).build())
        optionsBody.add(sectionHeader("Finish when done", muted = true))
        optionsBody.add(finishBox)
        for ((label, field, hint) in contractEditors()) {
            field.toolTipText = hint
            optionsBody.add(
                FormGrid().row(label, focusAccentScroll(field)).build()
            )
        }
        optionsBody.add(sectionHeader("Per-run budget (empty = machine/run defaults)", muted = true))
        optionsBody.add(budgetGrid.build())
        optionsBody.add(sectionHeader("Attachments (submitted as files)", muted = true))
        optionsBody.add(attachments)
        return optionsBody
    }

    private var optionsSheetHost: JPanel? = null

    private var optionsSheetPanel: JPanel? = null

    private var taskOptionsBackButton: JButton? = null

    private var optionsSheetReturnFocus: java.awt.Component? = null

    private fun toggleTaskOptionsSheet() {
        if (askModeSelected()) return
        if (taskOptionsSheetOpen()) {
            closeTaskOptionsSheet()
        } else {
            // Escape/Back returns focus to the exact trigger that opened the
            // sheet (the composer input keeps its draft meanwhile).
            optionsSheetReturnFocus = input.isFocusOwner.let { if (it) input else null }
            showTaskOptionsSheet()
        }
    }

    private fun showTaskOptionsSheet() {
        val host = optionsSheetHost ?: return
        taskOptionsButton.isSelected = true
        taskOptionsButton.text = "Task options ▾"
        (host.layout as CardLayout).show(host, "options")
        host.revalidate()
        host.repaint()
    }

    private fun closeTaskOptionsSheet() {
        val host = optionsSheetHost ?: return
        taskOptionsButton.isSelected = false
        taskOptionsButton.text = "Task options ▸"
        (host.layout as CardLayout).show(host, "conversation")
        val focus = optionsSheetReturnFocus ?: taskOptionsButton
        optionsSheetReturnFocus = null
        host.revalidate()
        host.repaint()
        focus.requestFocusInWindow()
    }

    /** True while the Task options sheet is the visible Work center surface. */
    private fun taskOptionsSheetOpen(): Boolean = optionsSheetPanel?.isVisible == true

    /** True while the Task options sheet hosts the Work center. */
    internal fun taskOptionsOpenForTest(): Boolean = taskOptionsSheetOpen()

    /**
     * One Inspect tab as ONE selected detail surface: a compact selector row
     * of [FaktorToggleButton]s (the existing pattern) above a card host that
     * shows exactly the selected detail. No stacked equal splits: the
     * non-selected details are simply not in the visible layout, so the
     * selected surface gets the whole tab body.
     */
    private fun selectorTab(title: String, details: List<InspectDetail>): JPanel {
        val host = JPanel(BorderLayout())
        val cards = CardLayout()
        val cardHost = JPanel(cards)
        for ((index, detail) in details.withIndex()) {
            cardHost.add(detail.view, index.toString())
        }
        cards.first(cardHost)
        val selector = JPanel(FlowLayout(FlowLayout.LEFT, Spacing.XS, 0))
        selector.isOpaque = false
        selector.border = BorderFactory.createEmptyBorder(Spacing.S, Spacing.M, Spacing.XS, Spacing.M)
        val group = ButtonGroup()
        for ((index, detail) in details.withIndex()) {
            val button = FaktorToggleButton(detail.label).apply {
                isSelected = index == 0
                addActionListener {
                    cards.show(cardHost, index.toString())
                    inspectSelection[title] = detail.label
                    host.revalidate()
                    host.repaint()
                }
            }
            group.add(button)
            selector.add(button)
        }
        inspectSelection[title] = details.first().label
        host.add(selector, BorderLayout.NORTH)
        host.add(cardHost, BorderLayout.CENTER)
        return host
    }

    /** The selector button labels of one Inspect tab. */
    internal fun inspectDetailLabelsForTest(tabTitle: String): List<String> {
        val selector = inspectSelectorFor(tabTitle) ?: return emptyList()
        return selector.components.filterIsInstance<JToggleButton>().map { it.text }
    }

    /**
     * Clicks one selector option by label; true when a button matched. The
     * selection goes through the ButtonGroup model (a headless test cannot
     * fire the real ActionEvent), then the button's own action runs.
     */
    internal fun selectInspectDetailForTest(tabTitle: String, label: String): Boolean {
        val selector = inspectSelectorFor(tabTitle) ?: return false
        for (button in selector.components.filterIsInstance<JToggleButton>()) {
            if (button.text == label) {
                button.isSelected = true
                button.actionListeners.forEach { listener ->
                    listener.actionPerformed(
                        java.awt.event.ActionEvent(
                            button,
                            java.awt.event.ActionEvent.ACTION_PERFORMED,
                            button.actionCommand
                        )
                    )
                }
                return true
            }
        }
        return false
    }

    /** The selected detail label of one Inspect tab. */
    internal fun selectedInspectDetailForTest(tabTitle: String): String =
        inspectSelection[tabTitle] ?: ""

    /** The number of detail surfaces visible in one Inspect tab (must be 1). */
    internal fun visibleInspectDetailCountForTest(tabTitle: String): Int {
        val cardHost = inspectCardHostFor(tabTitle) ?: return -1
        return cardHost.components.count { it.isVisible }
    }

    private fun inspectSelectorFor(tabTitle: String): JPanel? {
        val container = inspectTabContainer(tabTitle) ?: return null
        for (child in container.components) {
            if (child is JPanel && child.layout is FlowLayout) return child
        }
        return null
    }

    private fun inspectCardHostFor(tabTitle: String): JPanel? {
        val container = inspectTabContainer(tabTitle) ?: return null
        for (child in container.components) {
            if (child is JPanel && child.layout is CardLayout) return child
        }
        return null
    }

    private fun inspectTabContainer(tabTitle: String): java.awt.Container? {
        for (i in 0 until inspectTabs.tabCount) {
            if (inspectTabs.getTitleAt(i) == tabTitle) {
                return inspectTabs.getComponentAt(i) as? java.awt.Container
            }
        }
        return null
    }

    /**
     * Shows one utility view (Settings, Usage, Diagnostics) over the tabbed
     * navigator with an explicit Back affordance. The utilities are NOT
     * destinations: they are opened from the toolbar/overflow and dismissed
     * back to the Work/Inspect/History model.
     */
    private fun showUtility(title: String, panel: java.awt.Component) {
        val host = JPanel(BorderLayout())
        host.background = panelSurface()
        val backButton = secondaryButton("Back")
        backButton.addActionListener { hideUtility() }
        val headerRow = JPanel(BorderLayout(Spacing.S, 0))
        headerRow.isOpaque = true
        headerRow.background = panelSurface()
        headerRow.border = BorderFactory.createCompoundBorder(
            BorderFactory.createMatteBorder(0, 0, 1, 0, FaktorTheme.separator()),
            BorderFactory.createEmptyBorder(Spacing.S, Spacing.M, Spacing.S, Spacing.M)
        )
        val backRow = JPanel(FlowLayout(FlowLayout.LEFT, Spacing.S, 0))
        backRow.isOpaque = false
        backRow.add(backButton)
        headerRow.add(backRow, BorderLayout.WEST)
        utilityHeader.fullText = title
        utilityHeader.font = sectionTitleFont()
        utilityHeader.foreground = textForeground()
        headerRow.add(utilityHeader, BorderLayout.CENTER)
        host.add(headerRow, BorderLayout.NORTH)
        host.add(panel, BorderLayout.CENTER)
        contentHost.removeAll()
        contentHost.add(host, BorderLayout.CENTER)
        utilityShown = true
        contentHost.revalidate()
        contentHost.repaint()
    }

    private fun hideUtility() {
        contentHost.removeAll()
        contentHost.add(tabs, BorderLayout.CENTER)
        utilityShown = false
        contentHost.revalidate()
        contentHost.repaint()
    }

    internal fun utilityOpenForTest(): Boolean = utilityShown

    internal fun openUtilityForTest(title: String): Boolean {
        when (title) {
            "Settings" -> showUtility("Settings", settingsPanel)
            "Account", "Usage" -> openAccount()
            "Diagnostics" -> showUtility("Diagnostics", diagnosticsPanel)
            else -> return false
        }
        return true
    }

    /**
     * The explicit Account open: the ONLY trigger of the control-plane /
     * billing read. A disconnected installation renders the local empty
     * state without issuing a request.
     */
    private fun openAccount() {
        showUtility("Account", usagePanel)
        if (!accountLoaded) {
            accountLoaded = true
            if (!service.controlPlaneTokenConfigured()) {
                usagePanel.setNotConnected()
            } else {
                runAsync("account") { refreshUsagePanelBlocking() }
            }
        }
    }

    internal fun accountReadsForTest(): Int = accountReadAttempts

    internal fun accountLoadedForTest(): Boolean = accountLoaded

    internal fun refreshReadsForTest(): List<String> {
        val reads = ArrayList<String>()
        if (accountLoaded) reads.add("account")
        if (outcomesLoaded) reads.add("outcomes")
        if (service.currentSessionId() == null) return reads
        reads.add("status")
        reads.add("messages")
        reads.add("task-runs")
        reads.add("agents")
        reads.add("usage")
        reads.add("verification")
        reads.add("task-tree")
        reads.add("permissions")
        reads.add("terminals")
        reads.add("providers")
        reads.add("history")
        return reads
    }

    internal fun closeUtilityForTest() {
        hideUtility()
    }

    internal fun workPlaneForTest(): JPanel = workPlane

    /** The Work composer pane (host-matrix render hook). */
    internal fun taskComposerPaneForTest(): java.awt.Component = workPlane.getComponent(1)

    /**
     * Selects the cluster + sub-view that owns [panel] (used by evidence and
     * child-transcript navigation, which live in the Inspect cluster).
     */
    private fun selectInspectorPanel(panel: java.awt.Component) {
        val chain = findTabChain(tabs, panel) ?: return
        for ((pane, index) in chain) pane.selectedIndex = index
    }

    private fun findTabChain(
        pane: JTabbedPane,
        target: java.awt.Component
    ): List<Pair<JTabbedPane, Int>>? {
        for (i in 0 until pane.tabCount) {
            val component = pane.getComponentAt(i)
            if (component === target) return listOf(Pair(pane, i))
            if (component is JTabbedPane) {
                val nested = findTabChain(component, target)
                if (nested != null) return listOf(Pair(pane, i)) + nested
            }
            if (component is java.awt.Container) {
                val nested = findTabChainInChildren(component, target)
                if (nested != null) return listOf(Pair(pane, i)) + nested
            }
        }
        return null
    }

    private fun findTabChainInChildren(
        container: java.awt.Container,
        target: java.awt.Component
    ): List<Pair<JTabbedPane, Int>>? {
        for (child in container.components) {
            if (child === target) return emptyList()
            if (child is JTabbedPane) {
                val nested = findTabChain(child, target)
                if (nested != null) return nested
            }
            if (child is java.awt.Container) {
                val nested = findTabChainInChildren(child, target)
                if (nested != null) return nested
            }
        }
        return null
    }

    /** Selects one reachable view by title (smoke reachability). */
    internal fun selectPanelForTest(title: String): Boolean {
        if (tabs.getTitleAt(tabs.selectedIndex) == title) return true
        for (i in 0 until tabs.tabCount) {
            if (tabs.getTitleAt(i) == title) {
                tabs.selectedIndex = i
                return true
            }
            if (selectNestedTitle(tabs.getComponentAt(i), title)) {
                tabs.selectedIndex = i
                return true
            }
        }
        return false
    }

    private fun selectNestedTitle(component: java.awt.Component, title: String): Boolean {
        if (component !is JTabbedPane) return false
        for (i in 0 until component.tabCount) {
            if (component.getTitleAt(i) == title) {
                component.selectedIndex = i
                return true
            }
            if (selectNestedTitle(component.getComponentAt(i), title)) {
                component.selectedIndex = i
                return true
            }
        }
        return false
    }

    /**
     * Resolve one agent-control input ON the EDT and hand the value back to
     * the `faktor-ui` worker: the modal dialog never runs on (and never
     * blocks) the single worker, so abort/refresh work cannot queue behind a
     * pumping/modal LAf. The control call itself still runs on the worker,
     * and Swing is touched only on the EDT.
     */
    private fun agentInputOnEdt(title: String, initial: String?): String? {
        val show: () -> String? = {
            val provider = agentInputProvider
            if (provider != null) {
                provider(this, title, initial)
            } else {
                JOptionPane.showInputDialog(this, title, initial)
            }
        }
        if (SwingUtilities.isEventDispatchThread()) return show()
        val result = AtomicReference<String?>()
        val thrown = AtomicReference<Throwable?>()
        try {
            SwingUtilities.invokeAndWait {
                try {
                    result.set(show())
                } catch (t: Throwable) {
                    thrown.set(t)
                }
            }
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
            return null
        } catch (e: java.lang.reflect.InvocationTargetException) {
            thrown.set(e.targetException)
        }
        thrown.get()?.let { throw it }
        return result.get()
    }

    /**
     * The exact contract of the selected finish level, or null for the
     * DEFAULT path (Local — no conditional step requested), mirroring the
     * legacy "no contract is sent" semantics. The selector is a required
     * four-level group: its selection can never produce a non-hierarchical
     * combination (PR then Local simply selects Local and retains nothing).
     */
    internal fun completionContractFromControls(): NativeCompletionContract? =
        selectedFinishLevel().contract()

    /** The one selected finish level (Local is the default path). */
    internal fun selectedFinishLevel(): FinishLevel {
        val index = completionLevels.indexOfFirst { it.isSelected }
        return FinishLevel.values()[if (index < 0) 0 else index]
    }

    /** The four line-based editors of the Acceptance & contract disclosure. */
    private fun contractEditors(): List<Triple<String, JTextArea, String>> = listOf(
        Triple(
            "Constraints — conditions that cannot be violated",
            constraintField,
            "One constraint per line; at most 64 lines of 512 bytes each"
        ),
        Triple(
            "Invariants — properties that must remain true",
            invariantField,
            "One invariant per line; at most 64 lines of 512 bytes each"
        ),
        Triple(
            "Risks — likely ways the change could fail",
            riskField,
            "One risk per line; at most 64 lines of 512 bytes each"
        ),
        Triple(
            "Proof obligations — claims verification must evaluate",
            obligationField,
            "One claim per line; at most 64 lines of 512 bytes each"
        )
    )

    /** Split one editor into bounded rows; `second` is a refusal reason. */
    private fun parseContractLines(field: JTextArea, label: String): Pair<List<String>, String?> {
        val rows = ArrayList<String>()
        for (raw in field.text.split('\n')) {
            val value = raw.trim()
            if (value.isEmpty()) continue
            if (value.length > NativeContractRows.MAX_ROW_CHARS) {
                return Pair(
                    emptyList(),
                    "$label: a line exceeds ${NativeContractRows.MAX_ROW_CHARS} characters"
                )
            }
            if (value.toByteArray(Charsets.UTF_8).size > NativeContractRows.MAX_ROW_BYTES) {
                return Pair(
                    emptyList(),
                    "$label: a line exceeds ${NativeContractRows.MAX_ROW_BYTES} UTF-8 bytes"
                )
            }
            if (rows.size >= NativeContractRows.MAX_ROWS) {
                return Pair(emptyList(), "$label: more than ${NativeContractRows.MAX_ROWS} lines")
            }
            rows.add(value)
        }
        return Pair(rows, null)
    }

    /**
     * The bounded rows of the Acceptance & contract editors: null rows for
     * "nothing authored" (the default path). A bound violation returns a
     * typed refusal and the caller refuses the START loudly.
     */
    internal fun contractRowsCapture(): Pair<NativeContractRows?, String?> {
        val constraints = parseContractLines(constraintField, "constraints")
        constraints.second?.let { return Pair(null, it) }
        val invariants = parseContractLines(invariantField, "invariants")
        invariants.second?.let { return Pair(null, it) }
        val risks = parseContractLines(riskField, "risks")
        risks.second?.let { return Pair(null, it) }
        val obligations = parseContractLines(obligationField, "proof obligations")
        obligations.second?.let { return Pair(null, it) }
        val rows = NativeContractRows(
            constraints = constraints.first,
            invariants = invariants.first,
            risks = risks.first,
            proofObligations = obligations.first
        )
        return Pair(if (rows.isEmpty) null else rows, null)
    }

    /** The contract rows are per task start: reset after a success ack. */
    private fun resetContractRowControls() {
        constraintField.text = ""
        invariantField.text = ""
        riskField.text = ""
        obligationField.text = ""
    }

    /** The completion contract is per task start: reset after a success ack. */
    private fun resetCompletionControls() {
        completionLevels.first().isSelected = true
    }

    /**
     * The per-run budget of the options sheet: empty fields are the
     * machine/run defaults (null); an authored value must parse exactly like
     * the agent budget controls (positive decimal integer within the money
     * domain), else the start refuses loudly instead of silently dropping an
     * operator-authored bound.
     */
    private fun parseBudgetField(field: JTextField, label: String): Pair<BigInteger?, String?> {
        val raw = field.text.trim()
        if (raw.isEmpty()) return Pair(null, null)
        val parsed = MicroMoney.parseDecimal(raw)
        if (parsed == null || parsed.signum() <= 0) {
            return Pair(
                null,
                "$label \"$raw\": enter a positive decimal integer no larger than " +
                    MicroMoney.I64_MAX
            )
        }
        return Pair(parsed, null)
    }

    private fun requireBudgetControls(): Pair<BudgetCapture, String?> {
        val cost = parseBudgetField(budgetCostField, "max_cost_micro")
        cost.second?.let { return Pair(BudgetCapture(null, null), it) }
        val tokens = parseBudgetField(budgetTokensField, "max_tokens")
        tokens.second?.let { return Pair(BudgetCapture(null, null), it) }
        val tokenCount = tokens.first?.let { value ->
            if (value > BigInteger.valueOf(Long.MAX_VALUE)) {
                return Pair(
                    BudgetCapture(null, null),
                    "max_tokens \"${budgetTokensField.text.trim()}\": no larger than ${Long.MAX_VALUE}"
                )
            }
            value.toLong()
        }
        return Pair(BudgetCapture(tokenCount, cost.first), null)
    }

    /**
     * The attachment admission policy of the ACTIVE session's chosen model,
     * read from the daemon's advertised `/models` contract (the projection
     * names the active model when a run holds one, else the session's). Any
     * read failure falls back to the conservative emergency ceiling — the
     * client never mirrors the daemon's live numbers.
     */
    private fun resolveAttachmentPolicy(): AttachmentImages.Policy = try {
        val projection = service.projection()
        val active = projection.activeModel
        AttachmentImages.policyForModel(
            service.modelCatalog(),
            active?.provider ?: projection.provider,
            active?.model ?: projection.model
        )
    } catch (e: Exception) {
        AttachmentImages.emergencyPolicy()
    }

    /**
     * The Task composer's start path (also reachable from tests without an
     * EDT click): criteria, attachments, the settings mutation default and
     * the checked completion contract are captured into ONE immutable
     * submission at click time, then ride ONE native task-run request.
     *
     * Single-flight: the in-flight flag is claimed BEFORE the job reaches the
     * worker queue, so a double click / Enter race / burst of submits can
     * never enqueue a second start. The ONE per-submission id is the durable
     * idempotency identity the daemon replays after a lost response.
     */
    /** The selected composer intent (Ask is the default). */
    internal fun askModeSelected(): Boolean = askModeButton.isSelected

    internal fun intentForTest(): ComposerIntent =
        if (askModeSelected()) ComposerIntent.ASK else ComposerIntent.TASK

    /**
     * Clicks the Ask|Task intent toggle like the operator. The selection goes
     * through the ButtonGroup model (a headless test cannot fire the real
     * ActionEvent), then the ONE submit label/disclosure follows.
     */
    internal fun selectModeForTest(ask: Boolean) {
        val button = if (ask) askModeButton else taskModeButton
        button.isSelected = true
        button.actionListeners.forEach { listener ->
            listener.actionPerformed(
                java.awt.event.ActionEvent(
                    button,
                    java.awt.event.ActionEvent.ACTION_PERFORMED,
                    button.actionCommand
                )
            )
        }
        applyIntentPresentation()
    }

    /** The one submit label and the single Task-options disclosure. */
    private fun applyIntentPresentation() {
        val intent = if (askModeSelected()) ComposerIntent.ASK else ComposerIntent.TASK
        submitButton.text = intent.submitLabel()
        submitButton.a11y("Send", "Sends the composer with the ${intent.label()} intent.")
        val task = intent == ComposerIntent.TASK
        taskOptionsButton.isVisible = task
        taskOptionsButton.isEnabled = task && !startInFlight.get()
        if (!task && taskOptionsSheetOpen()) closeTaskOptionsSheet()
        taskOptionsButton.revalidate()
        taskOptionsButton.repaint()
    }

    /** The Run task / Ctrl+Enter dispatch, selected mode included. */
    private fun startTaskFromControls() {
        val goal = input.text.trim()
        if (goal.isEmpty()) {
            appendSystem("the composer is empty; describe what Faktor should do")
            return
        }
        if (askModeSelected()) {
            // Ask mode: the plain prompt only. The completion contract, the
            // contract rows, the finish level, the criteria, the per-run
            // budget and the attachments are NEVER attached.
            sendPrompt()
            return
        }
        // The authored contract rows are captured (and bounded) BEFORE the
        // in-flight claim: an oversized/malformed editor refuses the START
        // loudly, exactly like the daemon's typed 400, instead of silently
        // dropping a constraint the operator stated. The per-run budget is
        // validated the same way.
        val contractRows = contractRowsCapture()
        contractRows.second?.let { reason ->
            appendSystem("task start refused: $reason")
            return
        }
        val budget = requireBudgetControls()
        budget.second?.let { reason ->
            appendSystem("task start refused: $reason")
            return
        }
        // A settled Ask label never governs a Task run's strip; a LIVE Ask
        // turn keeps its reducer entry (and its Stop target).
        if (!turnLifecycleIsLive(turnLifecycle)) resetTurnLifecycle()
        if (!startInFlight.compareAndSet(false, true)) {
            appendSystem("a task start is already in flight; this submit was ignored")
            return
        }
        val draft = try {
            captureTaskStartDraft(goal, contractRows.first, budget.first)
        } catch (e: Throwable) {
            startInFlight.set(false)
            throw e
        }
        // A transport-failure retry of the SAME immutable draft reuses the
        // submission id (the daemon replays the original receipt); any other
        // start is a new logical submission with a fresh id.
        val sessionId = service.currentSessionId()
        val pending = pendingTaskSubmission
        val submission = if (
            pending != null &&
            pending.sessionId == sessionId &&
            pending.draft.sameDraftAs(draft)
        ) {
            pending
        } else {
            TaskStartSubmission(UUID.randomUUID().toString(), draft, sessionId)
        }
        // Host-journey observable: the logical send is captured before the job
        // reaches the worker queue, exactly like a click on Run task.
        taskStartObserver?.invoke(submission.submissionId, draft.goal)
        // Disable the start surface for the whole flight; the retry click
        // cannot even reach the queue while a start is pending.
        onEdt { setStartControlsEnabled(false) }
        // T07: the strip follows the start lifecycle immediately (the captured
        // draft is being admitted once); the next worker refresh re-reads the
        // served run row.
        refreshRunSummaryForClientState()
        try {
            runAsync("start task") { runTaskStart(submission) }
        } catch (e: Throwable) {
            startInFlight.set(false)
            throw e
        }
    }

    private fun captureTaskStartDraft(
        goal: String,
        contractRows: NativeContractRows?,
        budget: BudgetCapture
    ): TaskStartDraft {
        val criteria = criteriaField.text.split(',')
            .map { it.trim() }
            .filter { it.isNotEmpty() }
        return TaskStartDraft(
            goal = goal,
            criteria = if (criteria.isEmpty()) null else criteria,
            mutationMode = settingsPanel.mutationMode(),
            files = attachments.files(),
            binaries = attachments.binaryAttachments(),
            completionContract = completionContractFromControls(),
            contractRows = contractRows,
            budgetTokens = budget.tokens,
            budgetMaxCostMicro = budget.maxCostMicro
        )
    }

    private fun runTaskStart(submission: TaskStartSubmission) {
        if (service.currentSessionId() != submission.sessionId) {
            // The session switched between capture and dispatch (all jobs are
            // FIFO on one worker, so a queued open-session job can run
            // first): never durably start a run minted for the old session
            // against the new one. Mirrors the prompt path guard.
            startInFlight.set(false)
            onEdt {
                setStartControlsEnabled(true)
                appendSystem("session changed; the draft is kept")
            }
            refreshRunSummaryForClientState()
            return
        }
        val draft = submission.draft
        var succeeded = false
        var message: String
        try {
            // Binary + document parity (same representation as the VS Code
            // client): the ADVERTISED model contract governs every entry.
            // Deliverable documents (application/pdf, text/plain) upload as
            // durable binary attachments when the chosen model advertises
            // document input; allowlisted images — including in-memory
            // clipboard PNGs staged separately from paths — upload the same
            // way; ordinary workspace source files keep the repository
            // context (`files`) and are never blindly uploaded. An
            // undeliverable or oversize entry is a typed refusal BEFORE any
            // upload, so nothing partial reaches the durable store.
            val policy = resolveAttachmentPolicy()
            val plan = planAttachments(draft.files, draft.binaries, policy, workspaceRoot)
            // Upload plan in ENTRY order: binaries first, then image and
            // document paths. The retry key binds kind + mime + SHA-256 of
            // the exact bytes (the path is metadata only, never identity),
            // so a file whose bytes changed under the same path/size/mtime
            // uploads again, while an unchanged retry resolves the retained
            // id first and uploads only absent attachments (audit 29, CAS
            // dedupe foundation).
            val sessionId = try {
                service.projection().sessionId
            } catch (e: Exception) {
                ""
            }
            val uploadKeys = plan.uploads.map { it.key }
            val binary = pendingAttachmentRetry.resolve(sessionId, uploadKeys) { index ->
                val upload = plan.uploads[index]
                service.uploadAttachment(upload.mime, upload.filename, upload.base64)
            }
            val started = service.startTaskRun(
                draft.goal,
                draft.criteria,
                maxTokens = draft.budgetTokens,
                maxCostMicro = draft.budgetMaxCostMicro,
                mutationMode = draft.mutationMode,
                files = if (plan.pathFiles.isEmpty()) null else plan.pathFiles,
                attachments = if (binary.isEmpty()) null else binary,
                completionContract = draft.completionContract,
                submissionId = submission.submissionId,
                contractRows = draft.contractRows
            )
            // Durable acceptance: this submission's retained ids are no longer
            // pending; a later submission uploads from scratch.
            pendingAttachmentRetry.release(sessionId, uploadKeys)
            pendingTaskSubmission = null
            submittedCompletion = draft.completionContract
            succeeded = true
            val contractText = if (draft.completionContract == null) {
                ""
            } else {
                " completion=" +
                    draft.completionContract.requestedSteps().joinToString(",")
            }
            val attached = plan.pathFiles.size + plan.uploads.size
            message = "task run ${started.runId} started (${started.state})" +
                if (attached == 0) "" else " with " + plural(attached, "attachment") +
                contractText
        } catch (e: AttachmentRefusal) {
            pendingTaskSubmission = null
            lastFailure = UxFailure.PRE_RUN
            message = "error: start task: ${e.message}"
        } catch (e: NativeApiException) {
            // A lost start response is a pre-run failure: nothing is known to
            // have run and the retained draft offers the ONE Retry action.
            lastFailure = if (isTransportFailure(e)) UxFailure.PRE_RUN else classifyUxFailure(e)
            if (isTransportFailure(e)) {
                // The request may have reached the daemon; the retry MUST
                // present the same immutable submission id so the daemon
                // replays the original receipt instead of starting a second
                // run. The captured draft is retained for that retry.
                pendingTaskSubmission = submission
                message = "task start transport failure (${e.detail}); retrying reuses " +
                    "submission ${submission.submissionId}"
            } else {
                pendingTaskSubmission = null
                message = "task start refused: ${e.status} ${e.code}: ${e.detail}"
            }
        } catch (e: Exception) {
            pendingTaskSubmission = null
            lastFailure = UxFailure.PRE_RUN
            message = "error: start task: ${e.message ?: e.javaClass.simpleName}"
        } finally {
            startInFlight.set(false)
        }
        val succeededStart = succeeded
        if (succeededStart) lastFailure = null
        onEdt {
            setStartControlsEnabled(true)
            updateWorkStatus()
            appendSystem(message)
            if (succeededStart) {
                resetCompletionControls()
                resetContractRowControls()
                budgetTokensField.text = ""
                budgetCostField.text = ""
            }
        }
        // T07 ends with the result; T08 (the retained submission after a lost
        // response) renders until the retry resolves the same logical start.
        refreshRunSummaryForClientState()
        refreshTaskRunsBlocking()
        refreshTaskTreeBlocking()
    }

    // ------------------------------------------------ control-plane credential

    /**
     * The Settings-panel save path: the credential goes into the IDE
     * credential store (PasswordSafe in production) and NOWHERE else; the
     * non-secret coordinates and auth-session id are persisted separately, the
     * token field is already cleared, and the running client picks the token
     * up immediately (parity with the VS Code sign-in command).
     */
    private fun storeControlPlaneCredential(
        endpoint: String,
        organization: String,
        sessionId: String,
        token: String
    ) {
        val store = controlPlaneCredentials
        if (store == null) {
            settingsPanel.setControlPlaneStatus(
                "control plane: credential store unavailable in this embedding"
            )
            return
        }
        val scope = ControlPlaneScope(endpoint, organization)
        try {
            store.store(scope, token)
        } catch (e: IllegalArgumentException) {
            settingsPanel.setControlPlaneStatus("control plane: ${e.message}")
            return
        }
        val normalizedSession = sessionId.trim().ifEmpty { null }
        controlPlaneScopeStore?.write(scope)
        controlPlaneSessionStore?.write(normalizedSession)
        settingsPanel.setControlPlaneScope(scope)
        settingsPanel.setControlPlaneSession(normalizedSession)
        settingsPanel.clearControlPlaneTokenInput()
        service.setControlToken(store.resolve(scope))
        settingsPanel.setControlPlaneStatus(controlPlaneStatusText(store.resolve(scope)))
        appendSystem("control-plane credential stored in the IDE credential store")
    }

    /**
     * The Settings-panel sign-out path. The remote revoke is attempted FIRST
     * (`POST /native/sso/logout` needs the live token), then the credential
     * row is removed by COMPARE-AND-DELETE: only when it still holds the
     * exact credential this sign-out captured and revoked. An external
     * rotation during the in-flight revoke (another IDE / keychain UI) wrote
     * a NEW credential; that credential is left in place and reported typed,
     * never erased.
     */
    private fun signOutControlPlane() {
        val store = controlPlaneCredentials
        if (store == null) {
            settingsPanel.setControlPlaneStatus(
                "control plane: credential store unavailable in this embedding"
            )
            return
        }
        if (!signOutInFlight.compareAndSet(false, true)) {
            settingsPanel.setControlPlaneStatus("control plane: sign-out is already in progress")
            return
        }
        val scope = controlPlaneScopeStore?.read()
            ?: ControlPlaneScope(
                settingsPanel.controlPlaneEndpointText(),
                settingsPanel.controlPlaneOrganizationText()
            )
        val sessionId = controlPlaneSessionStore?.read()
            ?: settingsPanel.controlPlaneSessionText().ifEmpty { null }
        // Capture the exact credential (endpoint+organization key and token
        // value) this sign-out means to revoke and delete.
        val captured = if (scope.valid()) store.resolve(scope) else null
        settingsPanel.setSignOutEnabled(false)
        settingsPanel.setControlPlaneStatus("control plane: signing out…")
        // The remote revoke can block for the transport timeout: it runs on
        // the background worker, never on the EDT. All UI mutation returns
        // through onEdt; the button stays disabled until completion.
        runAsync("sign out") {
            try {
                val remote = revokeRemoteControlPlaneSession(
                    scope.takeIf { it.valid() },
                    sessionId,
                    captured,
                    signOutRevokeTimeoutMs
                )
                val outcome = when {
                    !scope.valid() -> ControlPlaneDeleteOutcome.ABSENT
                    captured == null -> if (store.delete(scope)) {
                        ControlPlaneDeleteOutcome.DELETED
                    } else {
                        ControlPlaneDeleteOutcome.ABSENT
                    }
                    else -> store.compareAndDelete(scope, captured)
                }
                if (outcome == ControlPlaneDeleteOutcome.ROTATED) {
                    // The stored credential rotated while the revoke was in
                    // flight: the CAPTURED token was the one revoked, and
                    // the NEW credential must survive and stay live.
                    val current = store.resolve(scope)
                    service.setControlToken(current)
                    onEdt {
                        settingsPanel.setControlPlaneStatus(
                            "control plane: credential rotated during sign-out; the NEW credential was kept"
                        )
                        appendSystem(
                            "control-plane sign-out: $remote; the credential was rotated by another window " +
                                "during sign-out, so the NEW credential was left in the IDE credential " +
                                "store and remains active"
                        )
                        finishSignOut()
                    }
                    return@runAsync
                }
                controlPlaneSessionStore?.write(null)
                service.setControlToken(null)
                val removed = outcome == ControlPlaneDeleteOutcome.DELETED
                onEdt {
                    settingsPanel.setControlPlaneSession(null)
                    settingsPanel.clearControlPlaneTokenInput()
                    settingsPanel.setControlPlaneStatus(
                        if (removed) {
                            "control plane: credential removed from the IDE credential store; remote: $remote"
                        } else {
                            "control plane: no credential was stored; remote: $remote"
                        }
                    )
                    appendSystem(
                        if (removed) {
                            "control-plane sign-out: $remote; the credential was removed from the IDE " +
                                "credential store"
                        } else {
                            "no stored control-plane credential to remove ($remote)"
                        }
                    )
                    finishSignOut()
                }
            } catch (e: Throwable) {
                onEdt {
                    val message = e.message ?: e.javaClass.simpleName
                    settingsPanel.setControlPlaneStatus("control plane: sign-out failed ($message)")
                    appendSystem("control-plane sign-out failed: $message")
                    finishSignOut()
                }
            }
        }
    }

    /** EDT-owned completion of one sign-out attempt (single-flight release). */
    private fun finishSignOut() {
        settingsPanel.setSignOutEnabled(true)
        signOutInFlight.set(false)
    }

    /**
     * The sign-out revoke. The daemon route names the `{organization,
     * session_id}` the presented token must own, so it is attempted ONLY when
     * the non-secret session id, a captured credential and a live client
     * exist; the CAPTURED token is presented (never the client's possibly
     * rotated one). A refusal is reported as the daemon typed it (401
     * non-owner, 404 unknown session, 409 cloud disabled).
     */
    private fun revokeRemoteControlPlaneSession(
        scope: ControlPlaneScope?,
        sessionId: String?,
        token: String?,
        timeoutMs: Long
    ): String {
        if (scope == null || sessionId.isNullOrBlank()) {
            return "no auth-session id is known, so the remote session was not revoked"
        }
        if (token == null) {
            return "no stored credential was found, so the remote session was not revoked"
        }
        if (!service.isRunning()) {
            return "the daemon is not running, so the remote session was not revoked"
        }
        return try {
            service.revokeControlPlaneSessionWithToken(
                scope.organization,
                sessionId,
                token,
                timeoutMs = timeoutMs
            )
            "the remote session was revoked"
        } catch (e: Exception) {
            "the remote revoke failed (${e.message})"
        }
    }

    /**
     * A PasswordSafe row change observed by the watcher: the live client gets
     * the new value; a removed row hands `null`, so the old token stops being
     * sent instead of lingering until a restart.
     */
    private fun applyWatchedControlPlaneToken(token: String?) {
        service.setControlToken(token)
        onEdt {
            settingsPanel.setControlPlaneStatus(
                if (token != null) {
                    "control plane: credential refreshed from the IDE credential store"
                } else {
                    "control plane: credential removed from the IDE credential store"
                }
            )
            appendSystem(
                if (token != null) {
                    "control-plane credential refreshed from the IDE credential store"
                } else {
                    "control-plane credential removed externally; the client credential was cleared"
                }
            )
        }
    }

    /**
     * Starts the bounded PasswordSafe row watch after the daemon is adopted;
     * idempotent. The first poll applies the vault value, so an external
     * update between panel construction and startup is never missed.
     */
    fun startControlPlaneWatch() {
        controlPlaneWatcher?.start()
    }

    private fun controlPlaneStatusText(token: String?): String =
        if (token != null) {
            "control plane: credential stored in the IDE credential store"
        } else {
            "control plane: no credential stored"
        }

    private fun wireActions() {
        startButton.addActionListener { startDaemon() }
        stopButton.addActionListener {
            runControl("stop daemon") {
                service.stop()
                submittedCompletion = null
                resetSessionView()
                onEdt {
                    setControlsEnabled(false)
                    appendSystem("daemon stopped")
                }
            }
        }
        newSessionButton.addActionListener {
            newSessionFromControls(selectedProviderOrDefault(), selectedModelOrDefault())
        }
        refreshButton.addActionListener {
            runAsync("refresh") { refreshAllBlocking() }
        }
        abortButton.addActionListener { stopActiveTurn() }
        submitButton.addActionListener { startTaskFromControls() }
        cancelRunButton.addActionListener {
            val run = runsCombo.selectedItem as? NativeTaskRun
            if (run == null) {
                appendSystem("select a task run first")
                return@addActionListener
            }
            runControl("cancel task run") {
                val ack = service.cancelTaskRun(run.runId)
                onEdt { appendSystem("cancel ${ack.runId}: cancelled=${ack.cancelled}") }
                refreshTaskRunsBlocking()
            }
        }
        treePanel.setListener(object : TaskTreePanel.Listener {
            override fun onEvidenceSelected(ref: EvidenceRef) {
                selectEvidenceRef(ref)
            }

            override fun onChildSelected(child: ChildNode) {
                loadChildTranscript(child)
            }
        })
        changesPanel.setListener(object : ChangesPanel.Listener {
            override fun onEvidenceSelected(ref: EvidenceRef) {
                selectEvidenceRef(ref)
            }

            override fun onOpenSourceDiff(model: ChangeReviewModel, rowIndex: Int?) {
                openImmutableChange(model, rowIndex)
            }

            override fun onOpenClaim(criterionKey: String) {
                openClaimInVerification(criterionKey)
            }

            override fun onReturnToRun() {
                selectCluster("Work")
                appendSystem("returned to the run surface")
            }

            override fun onRecheckBasis() {
                runAsync("recheck change basis") { recheckChangeBasisBlocking() }
            }
        })
        verificationPanel.setListener(object : VerificationPanel.Listener {
            override fun onEvidenceSelected(ref: EvidenceRef) {
                selectEvidenceRef(ref)
            }

            override fun onViewFinishActions() {
                selectCluster("Inspect")
                selectInspectDetail("Verification", "Verification")
            }
        })
        blockersPanel.setListener(object : BlockersPanel.Listener {
            override fun onBlockerAction(row: BlockerRow, action: BlockerAction) {
                runAsync("blocker ${action.label} ${row.childId}") {
                    when (action) {
                        BlockerAction.RESUME -> service.resumeAgent(row.childId)
                        BlockerAction.RETRY -> service.retryAgent(row.childId)
                        BlockerAction.CANCEL -> service.cancelAgent(row.childId)
                        BlockerAction.PERMISSION_ALLOW,
                        BlockerAction.PERMISSION_DENY -> {
                            val decision = if (action == BlockerAction.PERMISSION_ALLOW) "allow" else "deny"
                            val permission = permissionForChild(row.childId)
                            if (permission == null) {
                                onEdt {
                                    appendSystem(
                                        "no pending permission request for ${row.childId}; " +
                                            "resume the child or reply from the pending permissions list"
                                    )
                                }
                            } else {
                                replyPermissionBlocking(permission, decision)
                            }
                        }
                    }
                    refreshTaskTreeBlocking()
                    refreshAgentsBlocking()
                }
            }

            override fun onPermissionReply(permission: NativePermissionEntry, decision: String) {
                runControl("permission ${permission.id} $decision") {
                    replyPermissionBlocking(permission, decision)
                }
            }
        })
        agentsPanel.setListener(object : AgentsPanel.Listener {
            override fun onRefresh() {
                runAsync("refresh agents") { refreshAgentsBlocking() }
            }

            override fun onNotice(message: String) {
                onEdt { appendSystem(message) }
            }

            override fun onPause(agent: NativeAgent) {
                runAgentControl("Pause") { service.pauseAgent(agent.agentId) }
            }

            override fun onResume(agent: NativeAgent) {
                runAgentControl("Resume") { service.resumeAgent(agent.agentId) }
            }

            override fun onCancel(agent: NativeAgent) {
                runAgentControl("Cancel") { service.cancelAgent(agent.agentId) }
            }

            override fun onRetry(agent: NativeAgent) {
                runAgentControl("Retry") { service.retryAgent(agent.agentId) }
            }

            override fun onSteer(agent: NativeAgent) {
                runAgentControl("Steer") {
                    val text = agentInputOnEdt("Steer note for ${agent.agentId}", null)
                    if (text != null && text.isNotEmpty()) service.steerAgent(agent.agentId, text)
                }
            }

            override fun onSetModel(agent: NativeAgent) {
                runAgentControl("Model") {
                    val model = agentInputOnEdt("Model for ${agent.agentId}", agent.model ?: "")
                    if (model != null && model.isNotEmpty()) service.setAgentModel(agent.agentId, model)
                }
            }

            override fun onSetTokenBudget(agent: NativeAgent) {
                runAgentControl("Token budget") {
                    val raw = agentInputOnEdt("max_tokens for ${agent.agentId}", "10000")
                    val tokens = raw?.trim()?.toLongOrNull()
                    if (tokens != null && tokens > 0) service.setAgentBudget(agent.agentId, maxTokens = tokens)
                }
            }

            override fun onSetCostBudget(agent: NativeAgent) {
                runAgentControl("Cost budget") {
                    val raw = agentInputOnEdt("max_cost_micro for ${agent.agentId}", "1000000")
                    // Exact money: a plain decimal amount within i64::MAX; a
                    // junk or out-of-range value is refused loudly, never
                    // truncated.
                    val micro = raw?.trim()?.let { MicroMoney.parseDecimal(it) }
                    when {
                        raw == null -> Unit
                        micro == null || micro.signum() <= 0 ->
                            onEdt {
                                appendSystem(
                                    "refused max_cost_micro \"${raw.trim()}\": enter a positive decimal " +
                                        "integer no larger than ${MicroMoney.I64_MAX}"
                                )
                            }
                        else -> service.setAgentBudget(agent.agentId, maxCostMicro = micro)
                    }
                }
            }
        })
        usagePanel.setListener(object : UsagePanel.Listener {
            override fun onNextPage() {
                if (!usagePagingInFlight.compareAndSet(false, true)) return
                val next = usagePanel.model()?.nextCursor
                if (next == null) {
                    usagePagingInFlight.set(false)
                    return
                }
                billingPrevCursors.add(billingCursor ?: "")
                billingCursor = next
                runAsync("usage next page") { refreshUsagePanelBlocking() }
            }

            override fun onPreviousPage() {
                if (!usagePagingInFlight.compareAndSet(false, true)) return
                if (billingPrevCursors.isEmpty()) {
                    usagePagingInFlight.set(false)
                    return
                }
                val previous = billingPrevCursors.removeAt(billingPrevCursors.size - 1)
                billingCursor = previous.ifEmpty { null }
                runAsync("usage previous page") { refreshUsagePanelBlocking() }
            }

            override fun onGrantCredits() {
                val model = usagePanel.model()
                if (model == null || !model.grantEnabled()) {
                    appendSystem(model?.grantDisabledReason ?: "grant credits is not permitted")
                    return
                }
                // The dialog is a Swing mutation: it resolves ON the host UI
                // thread even when this listener is driven from a worker
                // callback that is not the EDT (audit: native dialogs never
                // run on a worker thread).
                val amountRaw = onEdtValue {
                    JOptionPane.showInputDialog(
                        this@FaktorChatPanel,
                        "Credit grant amount — $5.00 for dollars or 5000000µ$ for microUSD (exact)",
                        "$5.00"
                    )
                } ?: return
                // Explicit units only: a bare integer was previously
                // microUSD and is now ambiguous. "$5.00"/"5.00" are dollars;
                // "5000000µ$" is exact micro.
                val text = amountRaw.trim()
                val microMatch = Regex("^([0-9]+)\\s*(?:\u00b5\\$|micro|microusd)$", RegexOption.IGNORE_CASE)
                    .matchEntire(text)
                val usdMatch = Regex("^\\$?([0-9]+)(?:\\.([0-9]{1,6}))?$").matchEntire(text)
                val amount = when {
                    microMatch != null -> MicroMoney.parseDecimal(microMatch.groupValues[1])
                    usdMatch != null -> {
                        if (usdMatch.groupValues[2].isEmpty() && !text.startsWith("$")) {
                            null
                        } else {
                            val fraction = usdMatch.groupValues[2].padEnd(6, '0')
                            BigInteger(usdMatch.groupValues[1])
                                .multiply(BigInteger.valueOf(1_000_000L))
                                .add(BigInteger(fraction))
                        }
                    }
                    else -> null
                }
                if (amount == null || amount.signum() <= 0 || amount > MicroMoney.I64_MAX) {
                    appendSystem(
                        "grant refused: enter $5.00 for dollars or 5000000µ$ for microUSD " +
                            "(positive, exact, no larger than ${MicroMoney.I64_MAX}µ$)"
                    )
                    return
                }
                val organization = model.organization ?: "the organization"
                val confirmed = onEdtValue {
                    JOptionPane.showConfirmDialog(
                        this@FaktorChatPanel,
                        "Grant $${MicroMoney.usdText(amount)} (${MicroMoney.microText(amount)}) " +
                            "to $organization? This is a financial action.",
                        "Grant credits",
                        JOptionPane.OK_CANCEL_OPTION,
                        JOptionPane.WARNING_MESSAGE
                    ) == JOptionPane.OK_OPTION
                }
                if (!confirmed) return
                runAsync("grant credits") {
                    val ack = service.grantCredits(
                        amountMicro = amount,
                        idempotencyKey = java.util.UUID.randomUUID().toString(),
                        reason = "operator grant"
                    )
                    onEdt {
                        appendSystem(
                            if (ack.duplicate) {
                                "credit grant replayed (idempotent); the recorded grant is unchanged"
                            } else {
                                "credits granted; balance ${micro(ack.credits.balanceMicro())}"
                            }
                        )
                    }
                    refreshUsagePanelBlocking()
                }
            }

            override fun onExportUsage() {
                val organization = usagePanel.model()?.organization ?: return
                val target = onEdtValue {
                    val chooser = javax.swing.JFileChooser().apply {
                        dialogTitle = "Download Faktor usage"
                        selectedFile = java.io.File("faktor-usage-$organization.json")
                    }
                    if (chooser.showSaveDialog(this@FaktorChatPanel) ==
                        javax.swing.JFileChooser.APPROVE_OPTION
                    ) {
                        chooser.selectedFile
                    } else {
                        null
                    }
                } ?: return
                runAsync("download usage") {
                    val page = service.usageExport(organization)
                    onEdt {
                        target.writeText(page.toJson())
                        appendSystem(
                            "usage downloaded: ${page.items.size} row(s)" +
                                if (page.truncated) {
                                    " (bounded export; narrow the window with since)"
                                } else {
                                    ""
                                }
                        )
                    }
                }
            }

            override fun onChangePlan() {
                val model = usagePanel.model()
                if (model == null || !model.manageEnabled()) {
                    appendSystem(model?.manageDisabledReason ?: "changing a plan is not permitted")
                    return
                }
                val planId = onEdtValue {
                    JOptionPane.showInputDialog(
                        this@FaktorChatPanel,
                        "Plan id (must exist in the [billing] plan table)",
                        "pro"
                    )
                } ?: return
                val statuses = arrayOf("active", "trialing")
                val status = onEdtValue {
                    JOptionPane.showInputDialog(
                        this@FaktorChatPanel,
                        "Subscription status",
                        "Plan status",
                        JOptionPane.QUESTION_MESSAGE,
                        null,
                        statuses,
                        statuses[0]
                    ) as? String
                } ?: return
                val trialEndsMs = if (status == "trialing") {
                    val raw = onEdtValue {
                        JOptionPane.showInputDialog(
                            this@FaktorChatPanel,
                            "Trial end (milliseconds since the Unix epoch)",
                            "1800000000000"
                        )
                    } ?: return
                    raw.trim().toLongOrNull()?.takeIf { it >= 0 } ?: run {
                        appendSystem("plan change refused: the trial end must be a non-negative integer")
                        return
                    }
                } else {
                    null
                }
                val confirmed = onEdtValue {
                    JOptionPane.showConfirmDialog(
                        this@FaktorChatPanel,
                        "Change the organization plan to ${planId.trim()} ($status)? " +
                            "The change applies immediately; in-flight work is not interrupted.",
                        "Change plan",
                        JOptionPane.OK_CANCEL_OPTION,
                        JOptionPane.WARNING_MESSAGE
                    ) == JOptionPane.OK_OPTION
                }
                if (!confirmed) return
                runAsync("change plan") {
                    val changed = service.changeSubscription(
                        action = "change",
                        planId = planId.trim(),
                        status = status,
                        expiresMs = trialEndsMs,
                        idempotencyKey = java.util.UUID.randomUUID().toString()
                    )
                    onEdt {
                        appendSystem(
                            if (changed.replayed) {
                                "plan change replayed (idempotent); " +
                                    "${changed.subscription.planId} is unchanged"
                            } else {
                                "plan changed to ${changed.subscription.planId} " +
                                    "(${changed.subscription.status}); " +
                                    if (changed.proration.state == "available") {
                                        "proration applied"
                                    } else {
                                        "proration unavailable (${changed.proration.cause})"
                                    }
                            }
                        )
                    }
                    refreshUsagePanelBlocking()
                }
            }

            override fun onCancelSubscription() {
                val model = usagePanel.model()
                if (model == null || !model.manageEnabled()) {
                    appendSystem(model?.manageDisabledReason ?: "cancelling is not permitted")
                    return
                }
                val confirmed = onEdtValue {
                    JOptionPane.showConfirmDialog(
                        this@FaktorChatPanel,
                        "Cancel the organization subscription? New admissions will be denied " +
                            "immediately; in-flight work is not interrupted.",
                        "Cancel subscription",
                        JOptionPane.OK_CANCEL_OPTION,
                        JOptionPane.WARNING_MESSAGE
                    ) == JOptionPane.OK_OPTION
                }
                if (!confirmed) return
                runAsync("cancel subscription") {
                    val changed = service.changeSubscription(
                        action = "cancel",
                        confirm = true,
                        idempotencyKey = java.util.UUID.randomUUID().toString()
                    )
                    onEdt {
                        appendSystem(
                            if (changed.replayed) {
                                "cancellation replayed (idempotent); the subscription is already canceled"
                            } else {
                                "subscription canceled; new admissions are denied"
                            }
                        )
                    }
                    refreshUsagePanelBlocking()
                }
            }
        })
        tournamentPanel.setListener(object : TournamentPanel.Listener {
            override fun onLoadTournament(tournamentId: String) {
                runAsync("load tournament") {
                    val tournament = service.tournamentState(tournamentId)
                    onEdt { applyTournament(tournament) }
                }
            }

            override fun onStartTournament(goal: String, criteria: List<String>, n: Int) {
                runAsync("start tournament") {
                    val started = service.startTournament(goal, criteria, n)
                    trackedTournamentId = started.tournamentId
                    onEdt {
                        appendSystem(
                            "tournament ${started.tournamentId} started (${started.state}) " +
                                "candidates=${started.candidates.size}"
                        )
                    }
                    refreshTournamentsBlocking()
                    refreshTaskTreeBlocking()
                }
            }

            override fun onDecideTournament(tournamentId: String) {
                runAsync("decide tournament") {
                    try {
                        val decision = service.decideTournament(tournamentId)
                        onEdt {
                            appendSystem(
                                "tournament ${decision.tournamentId} decided: " +
                                    "winner=${decision.winner} " +
                                    "discarded=${decision.discarded.size}"
                            )
                        }
                    } catch (e: NativeApiException) {
                        // The engine rule (not all candidates settled / no
                        // eligible winner) surfaces as a typed refusal.
                        surfaceTournamentError("decide", e)
                    }
                    refreshTournamentsBlocking()
                    refreshTaskTreeBlocking()
                }
            }

            override fun onAbortTournament(tournamentId: String, reason: String) {
                runControl("abort tournament") {
                    try {
                        val aborted = service.abortTournament(tournamentId, reason)
                        onEdt { appendSystem("tournament ${aborted.id} aborted [${aborted.state}]") }
                    } catch (e: NativeApiException) {
                        surfaceTournamentError("abort", e)
                    }
                    refreshTournamentsBlocking()
                    refreshTaskTreeBlocking()
                }
            }
        })
        boardPanel.setListener(object : BoardPanel.Listener {
            override fun onRead() {
                runAsync("board read") { refreshBoardBlocking(acknowledge = true) }
            }

            override fun onPost(subject: String, body: String) {
                if (subject.isEmpty()) {
                    appendSystem("board post needs a subject")
                    return
                }
                if (body.isBlank()) {
                    appendSystem("board post needs a non-empty body")
                    return
                }
                runAsync("board post") {
                    try {
                        val post = service.boardPost(subject, body)
                        onEdt { appendSystem("board post #${post.revision} recorded") }
                    } catch (e: NativeApiException) {
                        // The durable authority owns every refusal (family
                        // scope, terminal child, oversized text): surface it
                        // typed instead of a silent no-op.
                        onEdt {
                            appendSystem(
                                "board post refused: ${e.status} ${e.code}: ${e.detail}"
                            )
                        }
                    }
                    refreshBoardBlocking(acknowledge = true)
                }
            }

            override fun onLoadOlder(beforeRevision: Long) {
                runAsync("board older page") {
                    try {
                        val page = service.board(since = beforeRevision, limit = 100L)
                        onEdt { boardPanel.appendOlderPage(page) }
                    } catch (e: NativeApiException) {
                        onEdt {
                            boardPanel.setPagingNote(
                                "older page refused (status ${e.status} ${e.code})"
                            )
                        }
                    } catch (e: Exception) {
                        onEdt {
                            boardPanel.setPagingNote(
                                "older page failed: ${e.message ?: e.javaClass.simpleName}"
                            )
                        }
                    }
                }
            }
        })
        navigator.setListener(object : EvidenceNavigatorPanel.Listener {
            override fun onRetrieve(evidenceId: Long, selectorJson: String) {
                runAsync("evidence $evidenceId") {
                    retrieveEvidenceIntoNavigator(evidenceId, selectorJson)
                }
            }

            override fun onMessageSelected(seq: Long) {
                onEdt { jumpToTranscript(seq) }
            }
        })
        permissionsPanel.setListener(object : PermissionsPanel.Listener {
            override fun onPermissionReply(permission: NativePermissionEntry, decision: String) {
                runControl("permission ${permission.id} $decision") {
                    replyPermissionBlocking(permission, decision)
                }
            }

            override fun onRefresh() {
                runAsync("refresh permissions") { refreshPermissionsBlocking() }
            }
        })
        workApprovalCard.setListener(object : WorkApprovalCard.Listener {
            override fun onReply(permission: NativePermissionEntry, decision: String) {
                runControl("permission ${permission.id} $decision") {
                    replyPermissionBlocking(permission, decision)
                }
            }

            override fun onRefresh() {
                runAsync("refresh permissions") { refreshPermissionsBlocking() }
            }
        })
        workApprovalCard.announcementSink = { text -> approvalAnnounceObserver?.invoke(text) }
        terminalPanel.setListener(object : TerminalPanel.Listener {
            override fun onRefresh() {
                runAsync("refresh terminals") { refreshTerminalsBlocking() }
            }

            override fun onSpawn(command: String, args: List<String>, cwd: String?) {
                runAsync("spawn terminal") {
                    val spawned = service.spawnTerminal(command, args, cwd)
                    onEdt {
                        terminalPanel.noteSpawned(spawned.ptyId, command, args, cwd)
                        appendSystem(
                            "terminal ${spawned.ptyId} spawned (pid ${spawned.pid}, " +
                                "op ${spawned.operationId})"
                        )
                    }
                    refreshTerminalsBlocking()
                }
            }

            override fun onOutput(ptyId: String) {
                runAsync("terminal output $ptyId") {
                    val output = service.terminalOutput(ptyId)
                    onEdt { terminalPanel.setOutput(output) }
                }
            }

            override fun onLoadEvents(after: Long) {
                runAsync("terminal events page") {
                    try {
                        val page = service.terminalEvents(after = after, limit = 64L)
                        onEdt { terminalPanel.appendEvents(page) }
                    } catch (e: NativeApiException) {
                        onEdt {
                            terminalPanel.setEventsNote(
                                "lifetime events page refused (status ${e.status} ${e.code})"
                            )
                        }
                    } catch (e: Exception) {
                        onEdt {
                            terminalPanel.setEventsNote(
                                "lifetime events page failed: ${e.message ?: e.javaClass.simpleName}"
                            )
                        }
                    }
                }
            }
        })
        settingsPanel.setListener(object : SettingsPanel.Listener {
            override fun onRefreshProviders() {
                runAsync("refresh providers") { refreshProvidersBlocking() }
            }

            override fun onProviderSelectionChanged(provider: String, model: String) {
                // The Settings selection is the single source of truth for the
                // next session's provider/model; nothing else mirrors it.
            }

            override fun onControlPlaneCredential(
                endpoint: String,
                organization: String,
                sessionId: String,
                token: String
            ) {
                storeControlPlaneCredential(endpoint, organization, sessionId, token)
            }

            override fun onControlPlaneLogout() {
                signOutControlPlane()
            }
        })
        historyPanel.setListener(object : HistoryPanel.Listener {
            override fun onOpenSession(sessionId: String) {
                runAsync("open session $sessionId") {
                    service.useSession(sessionId)
                    resetSessionView()
                    service.watchSession(sessionId, 0)
                    onEdt { appendSystem("session $sessionId opened from history") }
                    refreshAllBlocking()
                }
            }

            override fun onRefresh() {
                runAsync("refresh history") { refreshHistoryBlocking() }
            }

            override fun onRename(sessionId: String, title: String) {
                appendSystem(
                    "contract-required: POST /native/session/$sessionId/rename " +
                        "({title}) — the rename route is not currently served; no title was changed"
                )
            }

            override fun onPin(sessionId: String, pinned: Boolean) {
                appendSystem(
                    "contract-required: POST /native/session/$sessionId/pin ({pinned}) — " +
                        "the pin route is not currently served; no pin state was changed"
                )
            }

            override fun onArchive(sessionId: String) {
                appendSystem(
                    "contract-required: POST /native/session/$sessionId/archive — " +
                        "the archive route is not currently served; nothing was hidden"
                )
            }

            override fun onFork(sessionId: String) {
                appendSystem(
                    "contract-required: POST /native/session/$sessionId/fork — " +
                        "the fork route is not currently served; no branch was created"
                )
            }

            override fun onExport(sessionId: String) {
                appendSystem(
                    "contract-required: GET /native/session/$sessionId/export — " +
                        "the export route is not currently served; no file was written"
                )
            }
        })
        historyPanel.setTranscriptSearchListener { query ->
            runAsync("find in transcript") { searchTranscriptBlocking(query) }
        }
        outcomesPanel.setListener(object : OutcomesPanel.Listener {
            override fun onRefresh() {
                outcomesCursor = null
                outcomesPrevCursors.clear()
                runAsync("refresh outcomes") { refreshOutcomesBlocking() }
            }

            override fun onOlderPage() {
                if (!outcomesPagingInFlight.compareAndSet(false, true)) return
                val next = onEdtValue { outcomesPanel.model()?.nextCursor }
                if (next == null) {
                    outcomesPagingInFlight.set(false)
                    return
                }
                outcomesPrevCursors.add(outcomesCursor)
                outcomesCursor = next
                runAsync("outcomes older page") { refreshOutcomesBlocking() }
            }

            override fun onViewDetails() {
                // T29 `View details`: the Diagnostics utility carries the
                // daemon/authority readout; no monetary value is rendered in
                // the unavailable state itself.
                showUtility("Diagnostics", diagnosticsPanel)
            }
        })
        // The daemon/stream recovery machinery lives on Diagnostics, not on
        // History: reconnect resumes from the exact cursor, restart adopts a
        // restarted service, refresh re-reads the whole model.
        diagnosticsPanel.setListener(object : DiagnosticsPanel.Listener {
            override fun onRestart() {
                runAsync("restart daemon") {
                    val health = service.restart()
                    onEdt { appendSystem("daemon restarted: version ${health.version}") }
                    refreshAllBlocking()
                }
            }

            override fun onReconnect() {
                runAsync("reconnect stream") {
                    service.reconnectStream()
                    // The journal cursor is a Diagnostics detail (the
                    // connection readout), never ordinary transcript copy.
                    onEdt { appendSystem("Live updates reconnected") }
                    refreshHistoryBlocking()
                }
            }

            override fun onRefresh() {
                runAsync("refresh") { refreshAllBlocking() }
            }

            override fun onWaitForReconnect() {
                // T27 `Wait`: the reconnect loop keeps retrying from the last
                // valid journal position; waiting changes nothing and skips
                // nothing.
                appendSystem(
                    "live updates: still reconnecting; the last valid journal position is kept"
                )
            }

            override fun onRefreshFromSnapshot() {
                // T28 `Refresh from snapshot`: re-read the durable state
                // without touching the stream cursor; the blocked position is
                // never skipped.
                runAsync("refresh from snapshot") {
                    refreshAllBlocking()
                    onEdt {
                        appendSystem(
                            "refreshed from the durable snapshot; the last journal position is kept"
                        )
                    }
                }
            }

        })
    }

    /** Starts the daemon off the EDT (public so the app entry point can call it). */
    fun startDaemon() {
        // Settings are Swing controls: read them on the EDT once, then hand
        // the captured values to the worker (reading them off-EDT is a race).
        val provider = selectedProviderOrDefault()
        val model = selectedModelOrDefault()
        // The credential watch starts BEFORE the daemon: its first poll
        // applies the vault value, so an external PasswordSafe update between
        // panel construction and startup is reconciled, never missed.
        startControlPlaneWatch()
        // One start at a time from the UI: a queued duplicate click must not
        // spawn after a Stop (the service aborts internally too).
        onEdt { startButton.isEnabled = false }
        runAsync("start daemon") {
            onEdt { diagnosticsPanel.setReadiness("waiting", "starting the service") }
            try {
                val health = service.start()
                onEdt {
                    setControlsEnabled(true)
                    diagnosticsPanel.setReadiness("ready", "version ${health.version}")
                    appendSystem("daemon ready: version ${health.version}")
                    ensureEdition5SessionPanelsLoaded()
                }
                newSessionFromControls(provider, model)
            } catch (e: Throwable) {
                // The readiness wait ended without a ready service (resolver
                // refusal, spawn failure or readiness timeout): the typed
                // reason stays visible instead of a fabricated "ready".
                onEdt {
                    diagnosticsPanel.setReadiness(
                        "timeout",
                        e.message ?: e.javaClass.simpleName
                    )
                }
                throw e
            } finally {
                onEdt { startButton.isEnabled = true }
            }
        }
    }

    private fun selectedProviderOrDefault(): String =
        settingsPanel.selectedProvider() ?: "default"

    private fun selectedModelOrDefault(): String =
        settingsPanel.selectedModel() ?: "default"

    /**
     * The New-session composer path: the Settings provider selection (or the
     * composer text fields), one native session on the panel's workspace root
     * (the same root attachment paths are relativized against; the daemon
     * refuses absolute `files`, so both halves must agree), then the SSE
     * stream at cursor 0 and a full refresh.
     */
    private fun newSessionFromControls(provider: String, model: String) {
        if (startInFlight.get()) {
            appendSystem("a task start is in flight; wait for its result before starting a new task")
            return
        }
        runAsync("new session") {
            val created = service.createSession(
                provider, model, workspace = workspaceRoot?.toString(), title = "JetBrains session"
            )
            submittedCompletion = null
            // Reset is enqueued BEFORE the confirmation line: both are FIFO
            // onEdt calls, so the reset must not wipe the confirmation.
            resetSessionView()
            onEdt {
                appendSystem("session ${created.id} created (${created.title})")
            }
            service.watchSession(created.id, 0)
            refreshAllBlocking()
        }
    }

    private fun sendPrompt() {
        if (!service.isRunning()) {
            appendSystem("start the daemon first")
            return
        }
        val sessionId = service.currentSessionId()
        if (sessionId == null) {
            appendSystem("create a session first")
            return
        }
        val text = input.text.trim()
        if (text.isEmpty()) {
            appendSystem("cannot send: the prompt is empty")
            return
        }
        if (!promptInFlight.compareAndSet(false, true)) {
            appendSystem("a prompt is already in flight; wait for its result")
            return
        }
        // The host owns the logical prompt id: it is reused ONLY for a retry
        // of the byte-identical draft in the same session, so a lost response
        // replays the original receipt instead of duplicating a prompt.
        val pending = selectPromptSubmission(
            pendingPromptSubmission,
            sessionId,
            text
        ) { UUID.randomUUID().toString() }
        pendingPromptSubmission = pending
        // HTTP 2xx is admission, never completion: the reducer owns the phase
        // (and Stop) from here until durable authority settles the turn.
        dispatchTurn(TurnEvent.Submit(TurnSource.ASK))
        runAsync("prompt") {
            if (service.currentSessionId() != sessionId) {
                // The session switched between capture and dispatch: never
                // durably accept this prompt under the new session.
                promptInFlight.set(false)
                onEdt { appendSystem("session changed; the draft is kept") }
                return@runAsync
            }
            // SUBSCRIBE-BEFORE-SUBMIT: the live subscription's first frame is
            // the handshake snapshot at the exact sequence watermark, so a
            // fast reply can never outrun this listener. An older daemon that
            // does not serve live output answers typed `unavailable`; the
            // durable journal path keeps the product working.
            runCatching {
                service.watchLiveOutput(sessionId, awaitHandshakeMs = LIVE_HANDSHAKE_WAIT_MS)
            }
            try {
                val receipt = service.prompt(text, submissionId = pending.submissionId)
                pendingPromptSubmission = null
                clearUxFailure()
                onEdt {
                    // The admission receipt moves submitting → queued/starting;
                    // it NEVER completes the turn (the daemon may still be
                    // generating long after this 2xx).
                    dispatchTurn(
                        TurnEvent.Receipt(
                            opId = receipt.opId,
                            runId = null,
                            queued = receipt.queued
                        )
                    )
                    // Clear ONLY the exact submitted draft; text typed while
                    // the prompt was in flight is never destroyed.
                    if (input.text.trim() == pending.text) {
                        input.text = ""
                    }
                    // The optimistic user line carries the logical submission
                    // id: the durable twin reconciles with it by identity
                    // (one row, never two).
                    appendUser(pending.submissionId, pending.text)
                    appendSystem("message accepted (queued=${receipt.queued})")
                }
                // The plain Ask path refreshes ONLY the conversation surface:
                // no task-run, proof, verification, repo/index, usage or
                // billing read is caused by a question.
                refreshPromptPathBlocking()
            } catch (error: Exception) {
                // Retryable (transport/5xx/408/429): keep the draft AND the
                // id. Typed refusal: mint a new id on the next attempt.
                if (!promptFailureIsRetryable(error)) {
                    pendingPromptSubmission = null
                    // A typed refusal is a terminal admission outcome; a
                    // transport failure is UNKNOWN (the request may have been
                    // accepted), so the submission stays retryable.
                    onEdt { dispatchTurn(TurnEvent.TurnStatus(null, "failed")) }
                }
                noteUxFailure(error)
                onEdt { appendSystem("error: prompt: ${error.message}") }
            } finally {
                promptInFlight.set(false)
            }
        }
    }

    private fun promptFailureIsRetryable(error: Exception): Boolean {
        val api = error as? NativeApiException ?: return true
        return api.retryable ||
            api.status == 0 ||
            api.status == 408 ||
            api.status == 429 ||
            api.status >= 500
    }

    /**
     * Classify one observed failure into the shared cause vocabulary. The
     * mapper, not this classifier, decides precedence; this only names the
     * concrete cause with the wire's own status/code evidence.
     */
    private fun classifyUxFailure(error: Exception): UxFailure {
        val api = error as? NativeApiException ?: return UxFailure.UNRECOVERABLE
        val code = api.code.lowercase()
        if (api.status == 401 || api.status == 403 ||
            code.contains("credential") || code.contains("unauthorized") ||
            code.contains("forbidden") || code.contains("auth")
        ) {
            return UxFailure.CREDENTIAL
        }
        if (api.status == 0 || code == "transport" || code == "timeout") {
            return UxFailure.NETWORK
        }
        if (api.status == 408 || api.status == 429 || api.status >= 500 || api.retryable ||
            code.contains("rate") || code.contains("overload") || code.contains("transient")
        ) {
            return UxFailure.PROVIDER_TRANSIENT
        }
        return UxFailure.PRE_RUN
    }

    private fun noteUxFailure(error: Exception) {
        lastFailure = classifyUxFailure(error)
        onEdt { updateWorkStatus() }
    }

    private fun clearUxFailure() {
        if (lastFailure == null) return
        lastFailure = null
        onEdt { updateWorkStatus() }
    }

    // ----------------------------------------------------------- refreshers

    private fun refreshAllBlocking() {
        if (!service.isRunning()) return
        // The Account (billing/control-plane) surface is LAZY: a run refresh
        // never reads identity, entitlements, billing or usage. Only an
        // already-opened Account refreshes here; its own first open owns the
        // initial read. The outcomes ledger is lazy for the same reason.
        if (accountLoaded) refreshUsagePanelBlocking()
        if (outcomesLoaded) refreshOutcomesBlocking()
        if (service.currentSessionId() == null) return
        refreshStatusBlocking()
        refreshIndexCoverageBlocking()
        refreshMessagesBlocking()
        refreshTaskRunsBlocking()
        refreshAgentsBlocking()
        refreshUsageBlocking()
        refreshVerificationBlocking()
        refreshTournamentsBlocking()
        refreshBoardBlocking()
        refreshTaskTreeBlocking()
        refreshPermissionsBlocking()
        refreshTerminalsBlocking()
        refreshProvidersBlocking()
        refreshHistoryBlocking()
    }

    /**
     * The Ask path's bounded refresh: live conversation only. It never reads
     * task runs, task trees/proof/verification, the index (repo scan), usage,
     * identity, entitlements or billing.
     */
    private fun refreshPromptPathBlocking() {
        if (!service.isRunning() || service.currentSessionId() == null) return
        refreshStatusBlocking()
        refreshMessagesBlocking()
        refreshHistoryBlocking()
        refreshPermissionsBlocking()
        refreshTerminalsBlocking()
    }

    /**
     * Resolve one live pending permission. The reply carries the OWNING
     * session id from the entry ([NativePermissionEntry.sessionId]) — never
     * the panel's current session, which may not be the owner. The shared
     * [approvalGate] claimed the request id BEFORE the reply left the UI, so
     * a duplicate click could never emit a second reply; every exit path
     * releases it exactly once. The daemon's typed 409 refusals
     * (unknown/expired/already resolved, or a live waiter owned by another
     * session) are surfaced on BOTH decision surfaces (Work attention card
     * and Approvals list) with the typed reason, the request stays visible
     * and is never retried implicitly.
     */
    private fun replyPermissionBlocking(permission: NativePermissionEntry, decision: String) {
        var refusal: String? = null
        try {
            service.replyPermission(permission.sessionId, permission.id, decision)
            onEdt { appendSystem("permission ${permission.id}: $decision") }
        } catch (e: NativeApiException) {
            refusal = NativePermissionReplyRefusal.describe(e, permission.id)
                ?: "permission reply refused (status ${e.status} ${e.code}: ${e.detail})"
        } finally {
            // The typed result arrived (success or refusal): release the
            // per-request gate on every path before refreshing.
            approvalGate.finish(permission.id)
        }
        val text = refusal
        // Re-read the authority BEFORE recording the refusal so the refreshed
        // list cannot wipe the typed state; a refresh failure must not hide
        // the refusal either.
        try {
            refreshPermissionsBlocking()
            refreshTaskTreeBlocking()
        } catch (e: Exception) {
            onEdt {
                appendSystem("permission refresh failed: ${e.message ?: e.javaClass.simpleName}")
            }
        } finally {
            if (text != null) {
                onEdt {
                    appendSystem(text)
                    permissionsPanel.setReplyRefusal(permission.id, text)
                    workApprovalCard.setReplyRefusal(permission.id, text)
                }
            }
        }
    }

    /** Pending permissions of the session; an absent route is recorded. */
    private fun refreshPermissionsBlocking() {
        if (!service.isRunning() || service.currentSessionId() == null) return
        try {
            val permissions = service.permissions()
            onEdt {
                permissionsPanel.update(permissions)
                workApprovalCard.update(permissions)
            }
        } catch (e: NativeApiException) {
            onEdt {
                permissionsPanel.setUnavailable(
                    "permission read refused (status ${e.status} ${e.code}: ${e.detail})"
                )
            }
        } catch (e: Exception) {
            onEdt {
                permissionsPanel.setUnavailable(
                    "permission read failed: ${e.message ?: e.javaClass.simpleName}"
                )
            }
        }
    }

    /** Session-owned terminals + lifetime events from the native PTY routes. */
    private fun refreshTerminalsBlocking() {
        if (!service.isRunning() || service.currentSessionId() == null) return
        try {
            val page = service.terminals()
            onEdt { terminalPanel.update(page) }
        } catch (e: NativeApiException) {
            onEdt {
                terminalPanel.setUnavailable(
                    "terminal read refused (status ${e.status} ${e.code}: ${e.detail})"
                )
            }
            return
        } catch (e: Exception) {
            onEdt {
                terminalPanel.setUnavailable(
                    "terminal read failed: ${e.message ?: e.javaClass.simpleName}"
                )
            }
            return
        }
        try {
            val events = service.terminalEvents(limit = 64L)
            onEdt { terminalPanel.setEvents(events) }
        } catch (e: NativeApiException) {
            onEdt { terminalPanel.setEventsNote("lifetime events refused (status ${e.status} ${e.code})") }
        }
    }

    /**
     * Provider registry view; a daemon without the additive `/native/providers`
     * route degrades to the `/models` catalog (the same DTOs the tree joins
     * on), and only a total failure records an unavailable state.
     */
    private fun refreshProvidersBlocking() {
        if (!service.isRunning()) return
        try {
            val providers = service.providers()
            onEdt { settingsPanel.setProviders(providers) }
            return
        } catch (e: NativeApiException) {
            // Fall through to the catalog.
        } catch (e: Exception) {
            onEdt {
                settingsPanel.setUnavailable(
                    "provider read failed: ${e.message ?: e.javaClass.simpleName}"
                )
            }
            return
        }
        try {
            val catalog = service.modelCatalog()
            onEdt { settingsPanel.setCatalog(catalog) }
        } catch (e: Exception) {
            onEdt {
                settingsPanel.setUnavailable(
                    "provider read failed: ${e.message ?: e.javaClass.simpleName}"
                )
            }
        }
    }

    /** Durable prior work + the live stream/daemon readout. */
    private fun refreshHistoryBlocking() {
        if (!service.isRunning()) return
        try {
            val sessions = service.listSessions()
            val current = service.currentSessionId()
            val daemon = service.daemonDescription()
            val stream = service.streamStatus()
            val cursor = service.streamCursor()
            onEdt { historyPanel.update(sessions, current) }
            onEdt { diagnosticsPanel.setConnection(daemon, stream, cursor, current) }
            onEdt { syncSessionChooser(sessions, current) }
        } catch (e: NativeApiException) {
            onEdt {
                historyPanel.setUnavailable("history read refused (status ${e.status} ${e.code})")
            }
        } catch (e: Exception) {
            onEdt {
                historyPanel.setUnavailable(
                    "history read failed: ${e.message ?: e.javaClass.simpleName}"
                )
            }
        }
    }

    /**
     * The toolbar session chooser: the durable listing (title + served state,
     * never a fabricated date) with the current session kept selected across
     * snapshots when it is still served.
     */
    private fun syncSessionChooser(
        sessions: List<NativeSessionSummary>,
        currentId: String?
    ) {
        val selectedId = (sessionChooser.selectedItem as? NativeSessionSummary)?.id
        val previousCount = sessionChooserModel.size
        KeyedListSync.syncCombo(sessionChooserModel, sessions) { it.id }
        val preferred = selectedId ?: currentId
        if (preferred != null) {
            for (i in 0 until sessionChooserModel.size) {
                if (sessionChooserModel.getElementAt(i).id == preferred) {
                    sessionChooser.selectedIndex = i
                    break
                }
            }
        }
        if (sessionChooser.selectedIndex < 0 && sessionChooserModel.size > 0) {
            sessionChooser.selectedIndex = 0
        }
        updateSessionChooserState(previousCount)
    }

    private fun updateSessionChooserState(previousCount: Int) {
        sessionChooser.isEnabled = service.isRunning() && sessionChooserModel.size > 0
        openSessionButton.isEnabled =
            service.isRunning() && (sessionChooser.selectedItem as? NativeSessionSummary) != null
        if (previousCount != sessionChooserModel.size) {
            sessionChooser.revalidate()
            sessionChooser.repaint()
        }
    }

    /** The one-row rendering of a durable session: title · served state. */
    private inner class SessionChooserRenderer : javax.swing.DefaultListCellRenderer() {
        override fun getListCellRendererComponent(
            listComponent: javax.swing.JList<*>?,
            value: Any?,
            index: Int,
            selected: Boolean,
            focus: Boolean
        ): java.awt.Component {
            super.getListCellRendererComponent(listComponent, value, index, selected, focus)
            val session = value as? NativeSessionSummary ?: return this
            val title = session.title.trim().ifEmpty { "Untitled conversation" }
            text = ellipsizeIdentifier(title, 40) + " · " + ellipsizeIdentifier(session.state, 16)
            toolTipText = "$title · ${session.state}"
            return this
        }
    }

    /**
     * Opens the chooser's selection through the exact HistoryPanel open path:
     * `useSession` -> `resetSessionView` -> `watchSession(sessionId, 0)`, then
     * a full refresh. The draft survives (only session-owned views reset).
     */
    private fun openSelectedSession() {
        val session = sessionChooser.selectedItem as? NativeSessionSummary
        if (session == null) {
            appendSystem("select a durable session first")
            return
        }
        if (session.id == service.currentSessionId()) {
            appendSystem("session ${session.id} is already open")
            return
        }
        runAsync("open session ${session.id}") {
            service.useSession(session.id)
            resetSessionView()
            service.watchSession(session.id, 0)
            onEdt { appendSystem("session ${session.id} opened from the session chooser") }
            refreshAllBlocking()
        }
    }

    /**
     * The run summary strip's Inspect action: selects the Overview tab of the
     * Inspect cluster (never a new surface).
     */
    private fun inspectCurrentRun() {
        for (i in 0 until tabs.tabCount) {
            if (tabs.getTitleAt(i) == "Inspect") {
                tabs.selectedIndex = i
                break
            }
        }
        selectNestedTitle(inspectTabs, "Overview")
        selectInspectDetail("Overview", "Run details")
        appendSystem("inspecting the current run in Inspect")
    }

    private fun refreshStatusBlocking() {
        val projection = service.projection()
        onEdt { diagnosticsPanel.applyProjection(projection) }
        updateDaemonTrust()
    }

    /**
     * The Diagnostics trust row: the resolved executable and the daemon's own
     * checksum decision (D14 exact wording — "signed" is never used for the
     * bare `.sha256` record). A never-resolved binary stays `—`.
     */
    private fun updateDaemonTrust() {
        val resolved = service.lastDaemonTrust()
        onEdt {
            if (resolved == null) {
                diagnosticsPanel.setTrust("—", null)
            } else {
                diagnosticsPanel.setTrust(
                    daemonTrustLabel(resolved.trust),
                    resolved.executable.toString()
                )
            }
        }
    }

    /**
     * Durable index coverage of the current workspace (audits 5/6/16): read
     * best-effort so an older daemon (404) or a never-hosted index service
     * never breaks the status refresh; the label names the honest state
     * (including "not reported").
     */
    private fun refreshIndexCoverageBlocking() {
        if (!service.isRunning() || service.currentSessionId() == null) return
        try {
            val response = service.indexCoverage()
            onEdt { diagnosticsPanel.setIndexCoverage(coverageValue(indexCoverageLabel(response.snapshot))) }
        } catch (e: NativeApiException) {
            onEdt { diagnosticsPanel.setIndexCoverage("refused (${e.status} ${e.code})") }
        } catch (e: Exception) {
            onEdt { diagnosticsPanel.setIndexCoverage("read failed") }
        }
    }

    /** Strips the legacy `index: ` prefix from one coverage label. */
    private fun coverageValue(label: String): String =
        if (label.startsWith("index: ")) label.substring("index: ".length) else label

    private fun refreshMessagesBlocking() {
        // Capture the session before the read: a read that finishes after a
        // session switch must never append the old transcript into the new
        // session (the durable seq spaces are independent).
        val sessionAtRead = service.currentSessionId()
        val page = service.messages(limit = 50)
        onEdt {
            if (service.currentSessionId() != sessionAtRead) return@onEdt
            navigator.setMessages(page.messages)
            // Upsert by stable id + content revision: a message first seen
            // empty/partial still receives its later content (same seq), an
            // out-of-order arrival lands at its seq position, and the
            // optimistic user row reconciles with its durable twin by
            // identity instead of duplicating.
            val ops = transcriptModel.upsertDurable(page.messages) { key ->
                transcriptSink.locatedRow(key.render())
            }
            for (op in ops) {
                when (op) {
                    is TranscriptRowOp.Upsert -> transcriptSink.upsertRow(
                        op.key.render(), op.payload, op.beforeKey?.render()
                    )
                    is TranscriptRowOp.Rekey -> transcriptSink.rekeyRow(
                        op.key.render(), op.newKey.render(), op.payload, op.beforeKey?.render()
                    )
                }
            }
            // ONE document insert for a pure append batch; row updates edit
            // their exact ranges.
            transcriptSink.flush()
            // Reconcile the live suffix on top of the saved page immediately:
            // a durable flush that lags the stream never truncates visible
            // text, and a caught-up page never duplicates it.
            applyLiveOverlayOps()
        }
    }

    private fun refreshTaskRunsBlocking() {
        val sessionAtRead = service.currentSessionId()
        val runs = service.taskRuns()
        val views = try {
            service.taskViews()
        } catch (e: Exception) {
            emptyList<dev.faktor.shared.NativeTaskView>()
        }
        onEdt {
            if (service.currentSessionId() != sessionAtRead) return@onEdt
            // Preserve the selected run across an async refresh when the same
            // logical run is still served (a newer snapshot must not jump the
            // selection back to the first row).
            val selectedRunId = (runsCombo.selectedItem as? NativeTaskRun)?.runId
            KeyedListSync.syncCombo(runsModel, runs) { it.runId }
            if (selectedRunId != null) {
                val index = runs.indexOfFirst { it.runId == selectedRunId }
                if (index >= 0) runsCombo.selectedIndex = index
            }
            applyRunSummary(views.firstOrNull(), runs.firstOrNull()?.state)
        }
    }

    /**
     * The current-run strip: the served summary plus the T22 blocked-PR
     * surface. When the daemon serves the typed SCM-not-configured blocker
     * on the requested PR step, the notice (with its setup-guidance action)
     * becomes visible; PR success is never inferred from anything else.
     */
    private fun applyRunSummary(view: NativeTaskView?, runStateTag: String?) {
        currentRunView = view
        currentRunStateTag = runStateTag
        runSummaryArea.text = runSummaryText(view, runStatusBanner())
        updateWorkStatus()
        prSetupBody.isVisible = prScmSetupBlocked(view)
        prSetupBody.revalidate()
        prSetupBody.repaint()
    }

    /**
     * The compact Work status row. It carries only served facts: the design
     * run state, the active phase/run tag, exact token accounting and the
     * pending approval count. No estimate, no duplicated goal/plan.
     */
    private fun updateWorkStatus() {
        val ux = currentUxState()
        // The ONE turn reducer's display phase owns the live/settled label
        // (Sending/Queued/Starting/Generating/Working/Needs approval and the
        // settled Completed/Stopped/Failed outcomes); idle falls back to the
        // cause-specific UX state.
        val turnLabel = turnPhaseLabel(turnDisplayPhase(turnLifecycle))
        workStateChip.text = turnLabel ?: ux.label
        workStateChip.foreground = if (turnLabel == null) {
            semanticForeground(ux.tone)
        } else {
            semanticForeground(SemanticState.DIM)
        }
        workStateCopy.fullText = ux.copy
        workStateCopy.isVisible =
            ux.kind != UxStateKind.READY || currentRunView != null || currentRunStateTag != null
        val action = ux.actionLabel()
        workStateAction.text = action ?: "Wait"
        workStateAction.isVisible = action != null
        val tag = currentRunStateTag
        val phase = currentRunView?.phase?.takeIf { it.isNotBlank() }
        workRunLabel.text = phase ?: tag?.takeIf { it.isNotBlank() } ?: "No active run"
        workTokensLabel.text = tokenStatusText(currentRunView)
        updateApprovalStatus()
    }

    /**
     * The cause-specific state currently rendered. Signals are observed facts:
     * the last failure class, daemon lifecycle, stream state, pending input,
     * the served run tag and the failed-check count.
     */
    private fun currentUxState(): UxState = uxStateFor(currentUxSignals())

    private fun currentUxSignals(): UxSignals {
        val tag = currentRunStateTag
        val failure = lastFailure
        val tagRunning = tag?.equals("Running", ignoreCase = true) == true
        val tagBlocked = tag?.equals("Blocked", ignoreCase = true) == true
        val tagFailed = tag?.equals("Failed", ignoreCase = true) == true
        val tagCancelled = tag?.equals("Cancelled", ignoreCase = true) == true
        val verifying = currentProofTaskState.equals("verifying", ignoreCase = true) ||
            currentProofTaskState.equals("needs_verification", ignoreCase = true)
        return UxSignals(
            credentialFailure = failure == UxFailure.CREDENTIAL,
            unrecoverable = failure == UxFailure.UNRECOVERABLE,
            daemonReconnecting = daemonLifecycle == "failed" ||
                (daemonObservedRunning && daemonLifecycle == "stopped" &&
                    streamObservedOpen && lastStreamStatus == "retrying"),
            networkInterrupted = failure == UxFailure.NETWORK ||
                (streamObservedOpen && lastStreamStatus == "retrying" && !streamPaused),
            paused = streamPaused,
            preRunFailed = failure == UxFailure.PRE_RUN,
            providerRetrying = failure == UxFailure.PROVIDER_TRANSIENT,
            needsInput = pendingPermissions.isNotEmpty() || tagBlocked,
            checkFailed = (currentTree?.verification?.failedChecks ?: 0) > 0,
            starting = startInFlight.get(),
            checking = verifying,
            running = tagRunning || (currentRunView != null && tag == null),
            cancelled = tagCancelled,
            failed = tagFailed
        )
    }

    /** The ONE action a cause-specific state offers (no competing buttons). */
    private fun performUxAction(state: UxState) {
        when (state.action) {
            UxActionKind.OPEN_PROVIDER_SETTINGS -> {
                showUtility("Settings", settingsPanel)
                settingsPanel.focusProviders()
            }
            UxActionKind.RETRY -> startTaskFromControls()
            UxActionKind.RESUME -> runAsync("resume live updates") {
                service.reconnectStream()
                refreshAllBlocking()
            }
            UxActionKind.OPEN_DIAGNOSTICS -> showUtility("Diagnostics", diagnosticsPanel)
            UxActionKind.REPLY -> input.requestFocusInWindow()
            UxActionKind.INSPECT_VERIFICATION -> {
                selectCluster("Inspect")
                selectInspectDetail("Verification", "Verification")
            }
            UxActionKind.WAIT, UxActionKind.NONE -> Unit
        }
    }

    private fun tokenStatusText(view: NativeTaskView?): String {
        val budget = view?.budget ?: return "Tokens —"
        val spent = budget.spentTokens
        val max = budget.maxTokens
        val value = spent ?: max ?: return "Tokens —"
        val grouped = java.text.NumberFormat.getIntegerInstance().format(value)
        return if (spent != null && max != null && max > 0) {
            "Tokens $grouped / " + java.text.NumberFormat.getIntegerInstance().format(max)
        } else {
            "Tokens $grouped"
        }
    }

    private fun updateApprovalStatus() {
        val count = pendingPermissions.size
        workApprovalLabel.text =
            if (count == 0) "No approvals" else "$count approval" + (if (count == 1) "" else "s")
        workApprovalLabel.foreground =
            if (count == 0) mutedForeground() else semanticForeground(SemanticState.WARNING)
    }

    /**
     * The design run-status banner of the strip, in freshness order (see
     * [runStateViewForRun]): the CLIENT start lifecycle (T07 in flight / T08
     * response lost) wins over the served task-run state tag (T09 Running /
     * T11 Blocked awaiting input / T25 Cancelled / T26 Failed); a settled or
     * awaiting tag wins over the proof's T12 `Checking the change`
     * (`verifying` / `needs_verification` in the strict proof payload), which
     * in turn refines the collapsed `Running` tag. Done, Pending and Unknown
     * carry no design state of their own, so the proof's T12 may fill them in;
     * otherwise the strip renders its served facts only.
     */
    private fun runStatusBanner(): RunStateView? {
        val ux = currentUxState()
        if (ux.kind == UxStateKind.READY && currentRunView == null && currentRunStateTag == null) {
            return null
        }
        return RunStateView(ux.label, ux.copy, ux.actionLabel())
    }

    /**
     * Re-renders the strip from the RETAINED run facts and the current
     * client start state: T07/T08 change without a fresh served snapshot, so
     * the strip must follow the start lifecycle immediately (the next
     * refresh re-reads the served run row).
     */
    private fun refreshRunSummaryForClientState() {
        onEdt {
            runSummaryArea.text = runSummaryText(currentRunView, runStatusBanner())
            updateWorkStatus()
        }
    }

    /**
     * The compact run summary strip: only what the daemon actually served —
     * the design run-status banner first (when a state genuinely applies),
     * then the goal, phase, the candidate boundary (the completion gate's
     * requested steps), the state and the named steps (plan first, else
     * completion steps, else milestones). A missing member stays missing; no
     * "-"/"0" placeholder is ever invented.
     */
    private fun runSummaryText(view: NativeTaskView?, banner: RunStateView?): String {
        val summary = StringBuilder()
        fun line(text: String) {
            if (summary.isNotEmpty()) summary.append('\n')
            summary.append(text)
        }
        if (banner != null) {
            line(banner.label)
            line(banner.copy)
        }
        if (view == null) {
            line("no task run served")
            return summary.toString()
        }
        view.goal.takeIf { it.isNotEmpty() }?.let { line("goal: $it") }
        view.phase?.takeIf { it.isNotEmpty() }?.let { line("phase: $it") }
        val boundary = candidateBoundary(view)
        if (boundary.isNotEmpty()) line("candidate boundary: $boundary")
        line("state: ${view.state}")
        val steps = namedSteps(view)
        if (steps.isNotEmpty()) {
            line("steps: " + steps.joinToString(", ") { "${it.first}=${it.second}" })
        }
        return summary.toString()
    }

    /**
     * The candidate boundary of one run: the durable completion contract's
     * requested steps when the daemon serves one, else the milestones that
     * are DONE vs still open. Empty when nothing is served (stays missing).
     */
    private fun candidateBoundary(view: NativeTaskView): String {
        val completion = view.completion
        if (completion != null) {
            val requested = completion.contract.requestedSteps()
            if (requested.isNotEmpty()) return requested.joinToString(",")
        }
        val done = view.milestones.completed
        val open = view.milestones.open
        if (done.isEmpty() && open.isEmpty()) return ""
        return "${done.size} done, ${open.size} open"
    }

    private fun namedSteps(view: NativeTaskView): List<Pair<String, String>> {
        if (view.plan.isNotEmpty()) {
            return view.plan.map { Pair(it.summary, it.state) }
        }
        val completion = view.completion
        if (completion != null && completion.steps.isNotEmpty()) {
            return completion.steps.map { Pair(it.step, it.status) }
        }
        return view.milestones.completed.map { Pair(it, "done") } +
            view.milestones.open.map { Pair(it, "open") }
    }

    private fun refreshAgentsBlocking() {
        val sessionAtRead = service.currentSessionId()
        val agents = service.agents()
        onEdt {
            if (service.currentSessionId() != sessionAtRead) return@onEdt
            currentAgents = agents
            agentsPanel.update(agents, currentTree)
        }
    }

    private fun refreshUsageBlocking() {
        val sessionAtRead = service.currentSessionId()
        val usage = service.usage()
        val sessionUsage = service.sessionUsage()
        onEdt {
            if (service.currentSessionId() != sessionAtRead) return@onEdt
            diagnosticsPanel.setUsage(
                "usage: sessions=${usage.sessions} tokens=${sessionUsage.tokens} " +
                    "taskCostMicro=${usage.settledCostMicro}"
            )
        }
    }

    /**
     * The commercial-metering panel: the control-plane identity fixes the
     * organization, then one entitlement snapshot and one cursor page of the
     * usage fold. A typed refusal renders as the panel's explicit
     * disabled/unavailable state (billing_disabled → "billing disabled
     * locally") — never fabricated numbers. No session is required.
     */
    private fun refreshUsagePanelBlocking() {
        accountReadAttempts += 1
        if (!service.isRunning()) {
            usagePanel.setNotConnected()
            usagePagingInFlight.set(false)
            return
        }
        if (!service.controlPlaneTokenConfigured()) {
            usagePanel.setNotConnected()
            usagePagingInFlight.set(false)
            return
        }
        val cursor = billingCursor
        val hasPrev = billingPrevCursors.isNotEmpty()
        try {
            val identity = service.identity()
            val entitlements = service.entitlements()
            val usage = service.billingUsage(identity.organization, cursor, 50L)
            val overview = service.billingOverview()
            onEdt {
                usagePanel.setModel(
                    usagePanelModelOf(
                        identity, entitlements, usage, null, null, cursor, hasPrev, overview
                    )
                )
                usagePagingInFlight.set(false)
            }
        } catch (e: NativeApiException) {
            onEdt {
                if (e.status == 401 || e.status == 403 || e.status == 404) {
                    usagePanel.setNotConnected()
                } else {
                    usagePanel.setModel(
                        usagePanelModelOf(
                            null, null, null, e.code,
                            "${e.status} ${e.code}: ${e.detail}", cursor, hasPrev
                        )
                    )
                }
                usagePagingInFlight.set(false)
            }
        } catch (e: Exception) {
            onEdt {
                usagePanel.setModel(
                    usagePanelModelOf(
                        null, null, null, "unavailable",
                        e.message ?: e.javaClass.simpleName, cursor, hasPrev
                    )
                )
                usagePagingInFlight.set(false)
            }
        }
    }

    /**
     * Lazy first read of the outcomes ledger when the Outcomes surface is
     * first opened (and never before); a stopped daemon leaves the panel in
     * its honest not-read state.
     */
    private fun ensureOutcomesLoaded() {
        if (outcomesLoaded || !service.isRunning()) return
        if (historyTabs.selectedComponent !== outcomesPanel) return
        outcomesLoaded = true
        runAsync("outcomes") { refreshOutcomesBlocking() }
    }

    /**
     * The value-entitlements status line of the configured organization: the
     * typed report when the daemon can derive it, else the typed refusal
     * (`billing_disabled` renders "billing disabled locally"). Never throws —
     * the outcomes read itself decides the panel state.
     */
    private fun valueEntitlementsStatusFor(): String =
        try {
            valueEntitlementsStatusLine(service.valueEntitlements())
        } catch (e: NativeApiException) {
            valueEntitlementsRefusalStatusLine(e.code, "${e.status} ${e.code}: ${e.detail}")
        } catch (e: Exception) {
            valueEntitlementsRefusalStatusLine(null, e.message ?: e.javaClass.simpleName)
        }

    /**
     * One bounded newest-first outcomes page from the CURRENT cursor. A
     * refusal renders the T29 `Cost unavailable` state (no monetary rows)
     * with its TYPED code (`value_entitlement_refused` / `billing_disabled`
     * / …) and the value-entitlements status line; the conversations listing
     * is untouched (failure isolation).
     *
     * Entitlement surfacing: with a configured control-plane credential the
     * identity fixes the organization, the paid `outcome_analytics` read
     * passes `org=`, and the panel carries the value-entitlements report
     * line. Without a credential the frozen local read stands (no org, no
     * status line).
     */
    private fun refreshOutcomesBlocking() {
        if (!service.isRunning()) {
            outcomesPagingInFlight.set(false)
            return
        }
        val cursor = outcomesCursor
        val configured = service.controlPlaneTokenConfigured()
        try {
            val organization = if (configured) service.identity().organization else null
            val entitlementsStatus = if (configured) valueEntitlementsStatusFor() else null
            val page = service.outcomes(
                limit = OUTCOMES_PAGE_LIMIT,
                beforeMs = cursor?.completedAtMs,
                beforeRun = cursor?.runId,
                organization = organization
            )
            onEdt {
                outcomesPanel.setModel(
                    outcomesPanelModelOf(page, entitlementsStatus = entitlementsStatus)
                )
                outcomesPagingInFlight.set(false)
            }
        } catch (e: NativeApiException) {
            outcomesCursor = null
            val entitlementsStatus = if (configured) valueEntitlementsStatusFor() else null
            onEdt {
                outcomesPanel.setModel(
                    outcomesPanelModelOf(
                        null,
                        "outcomes read refused (status ${e.status} ${e.code}: ${e.detail})",
                        refusalCode = e.code,
                        entitlementsStatus = entitlementsStatus
                    )
                )
                outcomesPagingInFlight.set(false)
            }
        } catch (e: Exception) {
            outcomesCursor = null
            val entitlementsStatus = if (configured) valueEntitlementsStatusFor() else null
            onEdt {
                outcomesPanel.setModel(
                    outcomesPanelModelOf(
                        null,
                        "outcomes read failed: ${e.message ?: e.javaClass.simpleName}",
                        entitlementsStatus = entitlementsStatus
                    )
                )
                outcomesPagingInFlight.set(false)
            }
        }
    }

    private fun refreshVerificationBlocking() {
        val verification = service.verification()
        onEdt {
            diagnosticsPanel.setVerification(
                "verification: owed=${verification.owed.size} " +
                    "failed=${verification.failedChecks.size}"
            )
        }
    }

    // ---------------------------------------------------------------- tree

    /**
     * Auto-populates the tournament panel from the durable listing on every
     * session refresh: the tracked tournament when it still exists, else the
     * newest summary. Listing/load failures are surfaced as TYPED errors
     * (status/code/detail), never swallowed.
     */
    private fun refreshTournamentsBlocking() {
        if (!service.isRunning() || service.currentSessionId() == null) return
        val summaries = try {
            service.tournaments()
        } catch (e: NativeApiException) {
            surfaceTournamentError("listing", e)
            clearTrackedTournament()
            return
        } catch (e: Exception) {
            onEdt {
                appendSystem("tournament listing failed: ${e.message ?: e.javaClass.simpleName}")
            }
            clearTrackedTournament()
            return
        }
        onEdt { tournamentPanel.setSummaries(summaries) }
        val tracked = trackedTournamentId
        val target = if (tracked != null && summaries.any { it.id == tracked }) {
            tracked
        } else {
            latestTournamentId(summaries)
        }
        if (target == null) {
            trackedTournamentId = null
            trackedTournament = null
            onEdt { tournamentPanel.setTournament(null) }
            return
        }
        try {
            val tournament = service.tournamentState(target)
            onEdt { applyTournament(tournament) }
        } catch (e: NativeApiException) {
            surfaceTournamentError("load", e)
            clearTrackedTournament()
        } catch (e: Exception) {
            onEdt {
                appendSystem("tournament $target load failed: ${e.message ?: e.javaClass.simpleName}")
            }
            clearTrackedTournament()
        }
    }

    /** Drops the tracked tournament target and its rendered view. */
    private fun clearTrackedTournament() {
        trackedTournamentId = null
        trackedTournament = null
        onEdt { tournamentPanel.setTournament(null) }
    }

    private fun surfaceTournamentError(action: String, e: NativeApiException) {
        onEdt { appendSystem("tournament $action refused: ${e.status} ${e.code}: ${e.detail}") }
    }

    /**
     * The durable run-family board of the current session. A daemon without
     * the additive route (typed 404/405) records an explicit unavailable
     * state with its reason; the panel never shows a fabricated board.
     * `acknowledge` moves the read watermark (explicit read/post only).
     */
    private fun refreshBoardBlocking(acknowledge: Boolean = false) {
        val sessionAtRead = service.currentSessionId()
        if (!service.isRunning() || sessionAtRead == null) return
        try {
            val page = service.board(limit = 100L)
            onEdt {
                if (service.currentSessionId() != sessionAtRead) return@onEdt
                boardPanel.setBoard(page, acknowledge)
            }
        } catch (e: NativeApiException) {
            val routeMissing = e.status == 404 || e.status == 405 || e.status == 501
            val label = if (routeMissing) "no board route" else "board read refused"
            onEdt {
                if (service.currentSessionId() != sessionAtRead) return@onEdt
                boardPanel.setUnavailable("$label (status ${e.status} ${e.code}: ${e.detail})")
            }
        } catch (e: Exception) {
            onEdt {
                if (service.currentSessionId() != sessionAtRead) return@onEdt
                boardPanel.setUnavailable("board read failed: ${e.message ?: e.javaClass.simpleName}")
            }
        }
    }

    private fun refreshTaskTreeBlocking() {
        // The session this whole read belongs to: a read that finishes after
        // a session switch must never write its proof state into the freshly
        // reset strip.
        val sessionAtRead = service.currentSessionId()
        val projection = service.projection()
        var taskReadFailure: String? = null
        val task = try {
            service.taskViews().firstOrNull()
        } catch (e: Exception) {
            taskReadFailure = e.message ?: e.javaClass.simpleName
            null
        }
        val agents = service.agents()
        val graph = service.orchestratorGraph()
        val catalogNow = try {
            service.modelCatalog()
        } catch (e: Exception) {
            emptyList<NativeModelInfo>()
        }
        val verification = try {
            service.verification()
        } catch (e: Exception) {
            null
        }
        val runs = try {
            service.taskRuns()
        } catch (e: Exception) {
            emptyList<NativeTaskRun>()
        }
        val taskVerification = if (runs.isEmpty()) {
            null
        } else {
            try {
                service.taskVerification(runs[0].taskId.toString())
            } catch (e: Exception) {
                null
            }
        }
        // The strict proof summary feeds the top-level VERIFIED view. A
        // refusal is passed as an EXPLICIT unavailable reason (the cached
        // proof is dropped), so a stale VERIFIED can never render.
        var proofUnavailable: String? = null
        val proof = if (runs.isEmpty()) {
            null
        } else {
            try {
                service.taskProof(runs[0].taskId.toString())
            } catch (e: Exception) {
                proofUnavailable =
                    "proof read failed: " + (e.message ?: e.javaClass.simpleName)
                null
            }
        }
        // T12: the proof's served `task.state` is the ONLY source of the
        // checking state (the run tag collapses verifying/needs_verification
        // into `Running`). A refused/absent read clears it — never retained.
        val proofTaskState = proof?.taskState
        val sessionUsage = try {
            service.sessionUsage()
        } catch (e: Exception) {
            null
        }
        val childUsage = HashMap<String, dev.faktor.shared.NativeSessionUsage>()
        for (child in agents.filter { it.kind == "child" }.take(MAX_CHILD_USAGE_FETCHES)) {
            try {
                childUsage[child.sessionId.toString()] = service.sessionUsageFor(child.sessionId.toString())
            } catch (e: Exception) {
                // A child session without usage simply has no spend envelope.
            }
        }
        val tournament = trackedTournament
        val permissions = try {
            service.permissions()
        } catch (e: Exception) {
            emptyList<NativePermissionEntry>()
        }
        pendingPermissions = permissions
        edition5Attention.text = if (permissions.isEmpty()) {
            "No pending attention"
        } else {
            "${permissions.size} pending approval" + (if (permissions.size == 1) "" else "s")
        }
        edition5Attention.foreground = if (permissions.isEmpty()) {
            mutedForeground()
        } else {
            semanticForeground(SemanticState.WARNING)
        }
        updateApprovalStatus()
        val model = TaskTree.build(
            projection = projection,
            task = task,
            agents = agents,
            graph = graph,
            catalog = catalogNow,
            verification = verification,
            taskVerification = taskVerification,
            usage = sessionUsage,
            childUsage = childUsage,
            tournament = tournament,
            submittedCompletion = submittedCompletion,
            runState = runs.firstOrNull()?.state,
            proof = proof,
            proofUnavailable = proofUnavailable
        )
        onEdt {
            if (service.currentSessionId() != sessionAtRead) return@onEdt
            currentTree = model
            treePanel.update(model)
            blockersPanel.update(model.blockers, model.taskBlockers)
            tournamentPanel.setTournament(model.tournament)
            navigator.setEvidence(model.evidence)
            overviewPanel.updateModel(model)
            verificationPanel.updateModel(model)
            // I03: rows from served data only. When no change facts could be
            // read at all (the task view read failed and the projection
            // serves none), the surface reports the explicit read failure
            // instead of the empty-state copy; stale rows are never kept.
            val changeUnavailable = if (
                task == null && projection.filesChanged.isEmpty() && taskReadFailure != null
            ) {
                "change read failed: $taskReadFailure"
            } else {
                null
            }
            changesPanel.update(changeReviewOf(projection, task, taskVerification, changeUnavailable))
            agentsPanel.update(currentAgents, model)
            // T12: the proof read may refine the strip's collapsed Running
            // tag (or be refused, clearing a stale checking state). A read of
            // a session that is no longer current is dropped, and the strip
            // is only re-rendered from retained served facts — a reset strip
            // stays reset. The next applyRunSummary re-reads the state.
            if (service.currentSessionId() == sessionAtRead) {
                currentProofTaskState = proofTaskState
                if (currentRunView != null || currentRunStateTag != null) {
                    runSummaryArea.text = runSummaryText(currentRunView, runStatusBanner())
                    updateWorkStatus()
                }
            }
        }
    }

    private fun applyTournament(tournament: NativeTournament) {
        trackedTournamentId = tournament.id
        trackedTournament = tournament
        tournamentPanel.setTournament(TaskTree.tournamentView(tournament))
        appendSystem("tournament ${tournament.id} [${tournament.state}] winner=${tournament.winner ?: "-"}")
    }

    private fun retrieveEvidenceIntoNavigator(evidenceId: Long, selectorJson: String?) {
        val session = service.currentSessionId()
        val selector = selectorJson
            ?: dev.faktor.shared.NativeRequests.evidenceSelectorAll()
        try {
            val meta = service.evidence(evidenceId)
            val retrieval = service.retrieveEvidence(evidenceId, selector)
            val preview = safeTake(String(retrieval.bytes, Charsets.UTF_8), MAX_EVIDENCE_PREVIEW_CHARS)
            onEdt {
                if (service.currentSessionId() != session) return@onEdt
                navigator.showRetrieval(
                    evidenceId,
                    "id=${meta.id} retained=${meta.backingRetained} " +
                        "backingLen=${meta.backingLen ?: "-"} " +
                        "selector=${retrieval.selectorJson} " +
                        "truncated=${retrieval.truncatedByPolicy}\n" + preview,
                    retrieval.byteLen,
                    retrieval.truncatedByPolicy
                )
                appendSystem("evidence ${meta.id}: ${retrieval.byteLen} bytes retrieved")
            }
        } catch (e: Exception) {
            val message = e.message ?: e.javaClass.simpleName
            onEdt {
                if (service.currentSessionId() != session) return@onEdt
                navigator.showError(evidenceId, message)
                appendSystem("evidence $evidenceId error: $message")
            }
        }
    }

    /**
     * One evidence selection from ANY surface (tree or Changes): show the
     * Evidence detail, select the ref, then retrieve it on the worker (only
     * the network read runs off the EDT).
     */
    private fun selectEvidenceRef(ref: EvidenceRef) {
        onEdt {
            selectInspectorPanel(navigator)
            navigator.selectEvidence(ref)
        }
        if (ref.id != null) {
            runAsync("evidence ${ref.id}") {
                retrieveEvidenceIntoNavigator(ref.id, onEdtValue { navigator.selectorJson() })
            }
        }
    }

    /** Selects one top-level cluster by title (Work/Inspect/History). */
    private fun selectCluster(title: String) {
        for (i in 0 until tabs.tabCount) {
            if (tabs.getTitleAt(i) == title) {
                tabs.selectedIndex = i
                return
            }
        }
    }

    /**
     * I03 open: the pure controller resolves the immutable basis (candidate
     * `sourceDiffEvidence`, else the selected change's attached evidence
     * refs) and opens the served BYTES through the host seam. The session is
     * captured before the read; a session switch mid-read refuses typed
     * instead of opening foreign evidence.
     */
    private fun openImmutableChange(model: ChangeReviewModel, rowIndex: Int?) {
        val host = changeReviewHost
        if (host == null) {
            changesPanel.showOpenRefusal(
                "no IDE editor host is attached; the immutable basis cannot be opened in this environment"
            )
            return
        }
        runAsync("open source diff") {
            val session = service.currentSessionId()
            val controller = ChangeReviewController(host) { evidenceId ->
                if (service.currentSessionId() != session) {
                    throw dev.faktor.shared.NativeProtocolException(
                        "change review",
                        "the session changed during the immutable read"
                    )
                }
                service.retrieveEvidence(
                    evidenceId,
                    dev.faktor.shared.NativeRequests.evidenceSelectorAll()
                )
            }
            val result = controller.open(model, rowIndex)
            onEdt {
                when (result) {
                    is ChangeOpenResult.Opened -> changesPanel.clearOpenState()
                    is ChangeOpenResult.Unavailable -> changesPanel.showDiffUnavailable()
                    is ChangeOpenResult.Refused -> changesPanel.showOpenRefusal(result.reason)
                }
            }
        }
    }

    /**
     * I03 `Open claim`: select the related criterion in the Verification
     * surface when it is served; otherwise say so explicitly (never a
     * fabricated selection).
     */
    private fun openClaimInVerification(criterionKey: String) {
        selectInspectorPanel(verificationPanel)
        selectInspectDetail("Verification", "Verification")
        val selected = onEdtValue { verificationPanel.selectCriterion(criterionKey) }
        if (!selected) {
            appendSystem("no criterion \"$criterionKey\" is served in this run's verification")
        }
    }

    /** Clicks one Inspect selector option by label (production path). */
    private fun selectInspectDetail(tabTitle: String, label: String): Boolean {
        val selector = inspectSelectorFor(tabTitle) ?: return false
        for (button in selector.components.filterIsInstance<JToggleButton>()) {
            if (button.text == label) {
                button.doClick()
                return true
            }
        }
        return false
    }

    /**
     * S15 `Recheck current basis`: re-read the proof/projection and re-derive
     * the conflict facts against the last captured basis. There is no
     * rebase/apply control because no such capability exists.
     */
    private fun recheckChangeBasisBlocking() {
        if (!service.isRunning() || service.currentSessionId() == null) return
        val previous = onEdtValue { changesPanel.currentModel() }
        val projection = service.projection()
        val task = try {
            service.taskViews().firstOrNull()
        } catch (e: Exception) {
            null
        }
        val verification = try {
            val runs = service.taskRuns()
            if (runs.isEmpty()) null else service.taskVerification(runs[0].taskId.toString())
        } catch (e: Exception) {
            null
        }
        val current = changeReviewOf(projection, task, verification)
        val facts = basisConflictFacts(previous, current)
        onEdt {
            changesPanel.update(current)
            changesPanel.showConflictFacts(facts)
            appendSystem("basis recheck: " + facts.joinToString("; "))
        }
    }

    private fun permissionForChild(childId: String): NativePermissionEntry? {
        val child = currentTree?.children?.firstOrNull { it.childId == childId } ?: return null
        return pendingPermissions.firstOrNull { it.sessionId == child.sessionId.toString() }
    }

    private fun loadChildTranscript(child: ChildNode) {
        val session = service.currentSessionId()
        runAsync("child transcript ${child.childId}") {
            val page = service.messagesFor(child.sessionId.toString(), limit = 50)
            val text = page.messages.joinToString("\n") {
                "#${it.seq} ${it.role}: ${it.text}"
            }
            onEdt {
                if (service.currentSessionId() != session) return@onEdt
                navigator.showTranscriptSlice(
                    "child ${child.childId} (session ${child.sessionId})",
                    text.ifEmpty { "(no messages in the child transcript window)" }
                )
                selectInspectorPanel(navigator)
            }
        }
    }

    private fun jumpToTranscript(seq: Long) {
        val key = transcriptModel.keyForSeq(seq)
        if (key == null || !transcriptSink.locatedRow(key.render())) {
            appendSystem("message #$seq is outside the retained transcript window")
            return
        }
        try {
            transcriptView.requestFocusInWindow()
            // An explicit jump is user intent to follow again.
            transcriptPin.pin.onJump()
            transcriptView.scrollTo(key.render())
        } catch (e: Exception) {
            appendSystem("message #$seq cannot be focused: ${e.message}")
        }
    }

    // -------------------------------------------------------------- listener

    override fun onDaemonStatus(status: String, detail: String?) {
        onEdt {
            daemonLifecycle = status
            if (status == "running") {
                daemonObservedRunning = true
                lastFailure = null
                streamPaused = false
            }
            diagnosticsPanel.setDaemon("daemon: $status" + (if (detail == null) "" else " ($detail)"))
            diagnosticsPanel.setService(status, detail)
            when (status) {
                "starting" -> diagnosticsPanel.setReadiness("waiting", detail)
                "running" -> {
                    diagnosticsPanel.setReadiness("ready", detail)
                    updateDaemonTrust()
                }
                "failed" -> diagnosticsPanel.setReadiness("timeout", detail)
                else -> {}
            }
            updateWorkStatus()
        }
    }

    override fun onStreamStatus(status: String, detail: String?) {
        onEdt {
            lastStreamStatus = status
            if (status == "open") streamObservedOpen = true
            if (status == "open" || status == "connecting") {
                streamPaused = false
                if (lastFailure == UxFailure.NETWORK) lastFailure = null
            }
            diagnosticsPanel.setStream("stream: $status" + (if (detail == null) "" else " ($detail)"))
            diagnosticsPanel.setConnection(
                service.daemonDescription(), status, service.streamCursor(), service.currentSessionId()
            )
            when (status) {
                // T27: a transient reconnect is in progress; the cursor never
                // moves past the last delivered frame.
                "retrying" -> diagnosticsPanel.setStreamRecovery(StreamRecoveryState.RECONNECTING)
                // T28 is latched by onStreamBlocked with its typed detail; a
                // recovered/healthy status leaves it behind.
                "connecting", "open" -> diagnosticsPanel.setStreamRecovery(StreamRecoveryState.NONE)
                else -> Unit
            }
            updateWorkStatus()
        }
    }

    override fun onStreamBlocked(block: ProtocolBlocked) {
        // Deliver the frames already accepted before the block notice.
        streamPump.flushNow()
        onEdt {
            // Stable durable-stream block: the stream stopped reconnecting and
            // the cursor did not move past the offending event. Surface the
            // typed class, the T28 copy and the explicit recovery affordances
            // (reconnect / refresh from snapshot; nothing is skipped).
            streamPaused = true
            updateWorkStatus()
            diagnosticsPanel.setStreamRecovery(
                StreamRecoveryState.PAUSED,
                "${block.kind} at cursor ${block.cursor}: ${block.detail}"
            )
            appendSystem("Live updates paused: ${block.detail}")
            appendSystem("recovery: ${block.recovery.joinToString(" / ")}")
        }
    }

    /**
     * The stream thread NEVER touches Swing: it enqueues the frame and the
     * pump flushes a bounded batch on the EDT. This is the single per-delta
     * entry point, so 200 journal frames/s become at most 25 EDT flushes/s.
     */
    override fun onEvent(event: NativeSseEvent) {
        streamPump.offer(StreamDelta(service.currentSessionId(), event))
    }

    override fun onLiveMessage(message: LiveOutputMessage) {
        handleLiveMessage(message)
    }

    override fun onLiveStatus(status: String, detail: String?) {
        onEdt {
            diagnosticsPanel.setStream(
                "live: $status" + (if (detail == null) "" else " ($detail)")
            )
            if (status == "retrying" || status == "stopped" || status == "unavailable") {
                // A reconnect will re-handshake with a fresh snapshot:
                // everything buffered under the old connection is stale.
                livePump.discardPending()
            }
            updateWorkStatus()
        }
    }

    override fun onLiveError(message: String) {
        // Live transport diagnostics stay in Diagnostics: a malformed/foreign
        // frame or an older daemon without the route never writes a synthetic
        // transcript line (the durable journal remains the visible path).
        onEdt { diagnosticsPanel.setStream("live: $message") }
    }

    /**
     * ONE EDT render pass for a coalesced batch: stale-session frames and
     * replayed cursors are dropped, only durable milestones are written to
     * the transcript (routine model chunks stay off the screen-reader
     * channel) as HUMAN-READABLE status lines — the wire kind and journal
     * id land in the Diagnostics details surface, never as raw transcript
     * copy — the batch is applied, and the slow surfaces refresh at their
     * own bounded cadence.
     */
    private fun applyStreamBatch(batch: List<StreamDelta>) {
        val session = service.currentSessionId()
        val notable = ArrayList<StreamDelta>(minOf(batch.size, MAX_NOTABLE_EVENTS_PER_FLUSH))
        var accepted = 0
        for (delta in batch) {
            if (delta.sessionId != session) {
                uiMetrics.droppedStaleEvents.incrementAndGet()
                continue
            }
            if (delta.event.id <= lastAppliedEventSeq) {
                uiMetrics.duplicateEvents.incrementAndGet()
                continue
            }
            lastAppliedEventSeq = delta.event.id
            accepted++
            // The durable journal drives the ONE turn reducer: admission,
            // machine-state tags, tool phases and the terminal events.
            // Terminal transitions come ONLY from these durable events or the
            // turn record — never from a quiet stream or an HTTP 2xx.
            dispatchTurn(
                TurnEvent.Journal(
                    kind = delta.event.event,
                    opId = journalField(delta.event, "opId", "op_id"),
                    state = journalField(delta.event, "state")
                        ?: nestedState(delta.event)
                )
            )
            if (isNotableEvent(delta.event.event)) notable.add(delta)
        }
        if (accepted == 0) return
        for (delta in notable.takeLast(MAX_NOTABLE_EVENTS_PER_FLUSH)) {
            val copy = milestoneCopy(delta.event.event)
            if (delta.event.event.startsWith("permission_")) {
                // An approval decision is a TURN PART: it interleaves with
                // the text/tool parts by arrival order and joins the current
                // Faktor turn group (never its own anonymous line).
                transcriptSink.appendPart(
                    TranscriptRowPayload(
                        kind = TranscriptRowPayload.KIND_APPROVAL,
                        text = copy,
                        seq = delta.event.id,
                        rowId = "evt-${delta.event.id}",
                        group = transcriptModel.currentAssistantGroup() ?: ""
                    )
                )
            } else {
                transcriptSink.append(copy + "\n")
            }
            // The raw wire kind + journal id stay reachable in the details
            // surface, never in the ordinary transcript copy.
            onEdt {
                diagnosticsPanel.setStreamMilestone(
                    copy, delta.event.event, delta.event.id
                )
            }
        }
        transcriptSink.flush()
        scheduleStatusRefresh()
    }

    /** One top-level journal field (camelCase native form first). */
    private fun journalField(event: NativeSseEvent, vararg keys: String): String? {
        val record = event.data as? JsonValue.Obj ?: return null
        for (key in keys) {
            val value = (record.fields[key] as? JsonValue.Str)?.value
            if (value != null && value.isNotEmpty()) return value
        }
        return null
    }

    /** The machine-state tag may ride the journal `payload`. */
    private fun nestedState(event: NativeSseEvent): String? {
        val record = event.data as? JsonValue.Obj ?: return null
        val payload = record.fields["payload"] as? JsonValue.Obj ?: return null
        return (payload.fields["state"] as? JsonValue.Str)?.value
    }

    // ------------------------------------------------------- turn reducer

    /** Apply one reducer event on the EDT and re-render the state strip. */
    private fun dispatchTurn(event: TurnEvent) {
        if (!SwingUtilities.isEventDispatchThread()) {
            SwingUtilities.invokeLater { dispatchTurn(event) }
            return
        }
        turnLifecycle = turnReducer(turnLifecycle, event)
        val phase = turnDisplayPhase(turnLifecycle)
        val stopOp = turnStopOpId(turnLifecycle)
        val key = phase.wire + ":" + (stopOp ?: "")
        if (key != lastPostedTurnKey) {
            lastPostedTurnKey = key
            updateWorkStatus()
        }
    }

    private fun resetTurnLifecycle() {
        turnLifecycle = TurnLifecycle.empty()
        lastPostedTurnKey = null
        updateWorkStatus()
    }

    /**
     * Stop targets THE accepted operation the reducer tracks (never a queued
     * follow-up, never a bare "abort everything" when an op is known). The
     * typed abort ack is the authority that marks the turn `interrupted`.
     */
    private fun stopActiveTurn() {
        val stopOp = turnStopOpId(turnLifecycle)
        runControl("abort") {
            val ack = service.abort(stopOp)
            onEdt {
                val settled = stopOp ?: ack.aborted.firstOrNull()
                if (settled != null) dispatchTurn(TurnEvent.Stopped(settled))
                appendSystem("abort requested: ${ack.aborted}")
            }
        }
    }

    // ------------------------------------------------------- live output

    /**
     * One live-output message. Chunk deltas go through the bounded EDT pump;
     * the snapshot/turn/resync signals are authoritative and apply on the EDT.
     */
    private fun handleLiveMessage(message: LiveOutputMessage) {
        when (message) {
            is LiveOutputMessage.Chunk -> {
                val current = service.currentSessionId()
                if (current == null || message.frame.session.toString() != current) return
                livePump.offer(message.frame)
            }
            is LiveOutputMessage.Snapshot -> onEdt { applyLiveSnapshotMessage(message.snapshot) }
            is LiveOutputMessage.Turn -> onEdt {
                val turn = message.frame.turn ?: return@onEdt
                val status = message.frame.status ?: return@onEdt
                dispatchTurn(TurnEvent.TurnStatus(turn, status))
            }
            is LiveOutputMessage.Resync -> {
                // The stream client already fetched the authoritative snapshot
                // after a lag; a snapshot message follows. Never fabricate
                // text from the gap itself.
                onEdt { updateWorkStatus() }
            }
        }
    }

    /** The subscribe handshake and every reconnect baseline. */
    private fun applyLiveSnapshotMessage(snapshot: LiveSnapshot) {
        if (!SwingUtilities.isEventDispatchThread()) {
            SwingUtilities.invokeLater { applyLiveSnapshotMessage(snapshot) }
            return
        }
        val current = service.currentSessionId()
        if (current == null || snapshot.session.toString() != current) return
        transcriptModel.applyLiveSnapshot(snapshot)
        applyLiveOverlayOps()
        snapshot.turn?.let { dispatchTurn(TurnEvent.TurnStatus(it.op, it.status)) }
    }

    /**
     * ONE EDT pass for a coalesced live window: apply every delta to the
     * part-keyed overlay, then render the window once.
     */
    private fun applyLiveBatch(batch: List<LiveChunkFrame>) {
        val current = service.currentSessionId()
        var touched = false
        for (frame in batch) {
            if (current == null || frame.session.toString() != current) {
                uiMetrics.droppedStaleEvents.incrementAndGet()
                continue
            }
            if (transcriptModel.applyLiveChunk(frame)) touched = true
        }
        if (touched) applyLiveOverlayOps()
    }

    /** Render exactly the rows whose merged payload moved (one flush). */
    private fun applyLiveOverlayOps() {
        val ops = transcriptModel.liveOps()
        if (ops.isEmpty()) return
        for (op in ops) {
            when (op) {
                is TranscriptRowOp.Upsert -> transcriptSink.upsertRow(
                    op.key.render(), op.payload, op.beforeKey?.render()
                )
                is TranscriptRowOp.Rekey -> transcriptSink.rekeyRow(
                    op.key.render(), op.newKey.render(), op.payload, op.beforeKey?.render()
                )
            }
        }
        transcriptSink.flush()
    }

    /**
     * The explicit gap resync: fetch the authoritative part snapshot
     * (single-flight with a bounded cooldown) and reconcile. A saved page
     * never duplicates live text and never overwrites a newer live suffix.
     */
    private fun requestLiveResync(reason: String) {
        if (service.currentSessionId() == null || !service.isRunning()) return
        val now = System.currentTimeMillis()
        if (liveResyncInFlight.get() || now - liveResyncLastAt < LIVE_RESYNC_COOLDOWN_MS) return
        liveResyncLastAt = now
        liveResyncInFlight.set(true)
        runAsync("live resync") {
            try {
                val snapshot = service.liveSnapshot()
                onEdt { applyLiveSnapshotMessage(snapshot) }
            } catch (e: Exception) {
                onEdt { appendSystem("live resync ($reason): ${e.message ?: e.javaClass.simpleName}") }
            } finally {
                liveResyncInFlight.set(false)
            }
        }
    }

    /** Durable lifecycle milestones; per-chunk frames stay silent. */
    private fun isNotableEvent(kind: String): Boolean = when (kind) {
        "failed", "session_ended", "suspended", "resumed", "turn_completed",
        "permission_granted", "permission_denied", "permission_expired",
        "subagent_completed", "tool_cancelled", "context_compacted",
        "compact_rejected", "replay_started", "crash_detected", "recovery_applied" -> true
        else -> false
    }

    override fun onError(message: String) {
        onEdt { appendSystem("error: $message") }
    }

    /**
     * Marks the slow surfaces dirty and arms ONE delayed refresh. While a
     * refresh is in flight further flushes only set the dirty flag, so a hot
     * stream cannot queue a refresh per delta; the in-flight run re-arms once
     * it finishes (the flag is never lost, so the last delta always lands).
     */
    private fun scheduleStatusRefresh() {
        refreshDirty.set(true)
        if (SwingUtilities.isEventDispatchThread()) {
            if (!refreshDelayTimer.isRunning && !refreshQueued.get()) {
                refreshDelayTimer.restart()
            }
        } else {
            SwingUtilities.invokeLater { scheduleStatusRefresh() }
        }
    }

    private fun refreshIfDirty() {
        if (!refreshDirty.getAndSet(false)) return
        if (!refreshQueued.compareAndSet(false, true)) {
            refreshDirty.set(true)
            refreshDelayTimer.restart()
            return
        }
        worker.execute {
            try {
                if (service.isRunning() && service.currentSessionId() != null) {
                    refreshStatusBlocking()
                    refreshMessagesBlocking()
                    refreshBoardBlocking()
                    refreshTaskTreeBlocking()
                    uiMetrics.refreshRuns.incrementAndGet()
                }
            } catch (e: Exception) {
                // The stream may race a shutdown; errors surface via onError.
            } finally {
                refreshQueued.set(false)
                if (refreshDirty.get()) {
                    SwingUtilities.invokeLater { refreshDelayTimer.restart() }
                }
            }
        }
    }

    // ------------------------------------------------------------- plumbing

    fun shutdown() {
        // Detach FIRST: no late stream/status callback may touch a disposed
        // panel. Then stop the per-session scope (queued deltas are dropped),
        // the timers and the workers.
        service.setListener(null)
        controlPlaneWatcher?.stop()
        refreshDelayTimer.stop()
        streamPump.stop()
        livePump.stop()
        attachments.shutdown()
        sessionScope.disposeAll()
        service.stopStream()
        service.stopLiveOutput()
        worker.shutdownNow()
        controlWorker.shutdownNow()
    }

    // ------------------------------------------------ tool-window lifecycle

    /**
     * The tool-window lifecycle auto-start result (the host adapter seam,
     * called by the factory's trusted-install hook off the EDT): a
     * started/adopted service enables the same controls as the explicit
     * "Start service" action and surfaces the exact version; it never
     * fabricates a session. The panel itself never starts the daemon.
     */
    internal fun hostLifecycleStarted(version: String, adopted: Boolean) {
        onEdt {
            setControlsEnabled(true)
            // The transcript line lands BEFORE the readiness flip: a poller
            // that observes "ready" must never race an unwritten line.
            if (adopted) {
                appendSystem("Connected to the running Faktor service (version $version)")
            } else {
                appendSystem("daemon ready: version $version")
            }
            diagnosticsPanel.setReadiness("ready", "version $version")
            ensureEdition5SessionPanelsLoaded()
        }
    }

    /**
     * A typed auto-start refusal surfaces in Diagnostics and the transcript
     * (human copy; the machine kind stays in the readiness detail) and
     * leaves the explicit "Start service" action as the only start path.
     */
    internal fun hostLifecycleRefused(kind: String, detail: String) {
        onEdt {
            // Transcript line first, readiness second (see hostLifecycleStarted).
            appendSystem(autoStartRefusalCopy(kind, detail))
            diagnosticsPanel.setReadiness("auto-start refused", "$kind · $detail")
        }
    }

    /** One synchronous credential-watch poll (internal smoke hook). */
    internal fun pollControlPlaneWatcherOnce(): Boolean =
        controlPlaneWatcher?.pollOnce() ?: false

    /**
     * Wait (from a non-EDT test thread) until the background sign-out has
     * finished and its final EDT updates have run.
     */
    internal fun awaitSignOutIdleForTest(timeoutMs: Long = 5_000L): Boolean {
        check(!SwingUtilities.isEventDispatchThread()) {
            "awaitSignOutIdleForTest must be called off the EDT"
        }
        val deadline = System.nanoTime() + TimeUnit.MILLISECONDS.toNanos(timeoutMs)
        while (signOutInFlight.get()) {
            if (System.nanoTime() >= deadline) return false
            Thread.sleep(2)
        }
        SwingUtilities.invokeAndWait { }
        return true
    }

    /** The sign-out button's enabled state (single-flight, smoke hook). */
    internal fun signOutEnabledForTest(): Boolean = settingsPanel.signOutEnabled()

    /** EDT/work counters after any workload (perf harness + smoke). */
    internal fun edtMetricsForTest(): UiWorkSnapshot = uiMetrics.snapshot()

    /** Frames waiting for the next coalesced flush. */
    internal fun pendingStreamDeltasForTest(): Int = streamPump.pending

    /**
     * One durable-message refresh, drained to the EDT before returning
     * (deterministic transcript-upsert seam; production goes through the
     * worker + cadence timers).
     */
    internal fun refreshMessagesForTest() {
        if (!service.isRunning() || service.currentSessionId() == null) return
        if (SwingUtilities.isEventDispatchThread()) {
            refreshMessagesBlocking()
            return
        }
        refreshMessagesBlocking()
        SwingUtilities.invokeAndWait { }
    }

    /** The durable transcript model's stale-snapshot refusals (seam). */
    internal fun transcriptRevisionGuardHitsForTest(): Long =
        onEdtValue { transcriptModel.revisionGuardHits }

    /** The rendered transcript row keys in document order (seam). */
    internal fun transcriptRowKeysForTest(): List<String> = onEdtValue {
        transcriptModel.rows().map { it.key.render() }
    }

    /**
     * Flush queued stream frames synchronously on the EDT (deterministic
     * smoke/harness hook; production always goes through the timer).
     */
    internal fun flushStreamForTest() {
        if (SwingUtilities.isEventDispatchThread()) {
            streamPump.flushNow()
        } else {
            SwingUtilities.invokeAndWait { streamPump.flushNow() }
        }
    }

    /** Deliver one live-output message exactly like the service listener. */
    internal fun onLiveMessageForTest(message: LiveOutputMessage) {
        handleLiveMessage(message)
    }

    /** Apply every queued live chunk in ONE EDT flush (seam). */
    internal fun flushLiveForTest() {
        onEdtValue { livePump.flushNow() }
    }

    /** Apply a reducer event synchronously on the EDT (seam). */
    internal fun dispatchTurnForTest(event: TurnEvent) {
        onEdtValue { dispatchTurn(event) }
    }

    internal fun turnPhaseForTest(): String =
        onEdtValue { turnDisplayPhase(turnLifecycle).wire }

    internal fun turnStopOpForTest(): String? =
        onEdtValue { turnStopOpId(turnLifecycle) }

    internal fun turnIsLiveForTest(): Boolean =
        onEdtValue { turnLifecycleIsLive(turnLifecycle) }

    internal fun liveGapSignalsForTest(): Long =
        onEdtValue { transcriptModel.liveGapSignals }

    internal fun liveForeignDroppedForTest(): Long =
        onEdtValue { transcriptModel.liveForeignDropped }

    internal fun liveDuplicatesDroppedForTest(): Long =
        onEdtValue { transcriptModel.liveDuplicatesDropped }

    /** The live part-keyed overlay (raw text + UTF-8 byte counter). */
    internal fun livePartsForTest(): Map<String, String> = onEdtValue {
        transcriptModel.livePartsForTest().mapValues { (_, part) -> part.text + "@" + part.bytes }
    }

    internal fun workStateChipTextForTest(): String = onEdtValue { workStateChip.text }

    /** Invoke the Stop/Abort action exactly as a host gesture would. */
    internal fun triggerAbortForTest() {
        onEdtValue { stopActiveTurn() }
    }

    /** True while the transcript follows appended content. */
    internal fun transcriptPinnedForTest(): Boolean = transcriptPin.pin.isPinned

    /** The pure scroll-pin state (pinned/detached). */
    internal fun scrollPinStateForTest(): ScrollPin.State = transcriptPin.pin.stateForTest

    /** Simulate a user scrollbar move for the follow/detach contract. */
    internal fun userScrolledTranscriptForTest(value: Int, maximum: Int, extent: Int) {
        transcriptPin.pin.onUserScroll(value, maximum, extent)
    }

    /** The current ephemeral session scope disposal count. */
    internal fun sessionScopesDisposedForTest(): Long =
        uiMetrics.snapshot().sessionScopesDisposed

    // ------------------------------------------------------------- test hooks
    // Internal (module-visible) seams used by the Kotlin parity smoke to
    // drive the REAL panel wiring against canned frames / a fake daemon
    // without a display. Every hook calls the same code path as its control.

    internal fun submitTaskForTest() {
        // The seam is explicitly the Task intent; Ask has its own seam.
        if (askModeSelected()) selectModeForTest(ask = false)
        startTaskFromControls()
    }

    internal fun submitAskForTest() {
        if (!askModeSelected()) selectModeForTest(ask = true)
        sendPrompt()
    }

    // ---------------------------------------------------- UX test seams

    /** The compact status row's exact visible words (state, run, tokens, approvals). */
    internal fun workStatusTextForTest(): List<String> =
        listOf(workStateChip.text, workRunLabel.text, workTokensLabel.text, workApprovalLabel.text)

    internal fun workStatusComponentsForTest(): List<java.awt.Component> =
        listOf(workStateChip, workRunLabel, workTokensLabel, workApprovalLabel)

    /** The ONE submit action and its Ask-first default. */
    internal fun submitLabelForTest(): String = submitButton.text

    internal fun submitActionCountForTest(): Int = 1

    internal fun taskOptionsVisibleForTest(): Boolean = taskOptionsButton.isVisible

    /** The cause-specific state copy + exact action label currently rendered. */
    internal fun workStateCopyForTest(): String = workStateCopy.fullText

    internal fun workStateActionForTest(): String? =
        workStateAction.text.takeIf { workStateAction.isVisible }

    /** Normal navigation must not expose the developer screen registry. */
    internal fun normalNavigationLabelsForTest(): List<String> {
        val labels = ArrayList<String>()
        for (i in 0 until tabs.tabCount) labels.add(tabs.getTitleAt(i))
        labels.addAll(tabTitles())
        labels.addAll(actionControlsForTest().values)
        labels.add(workDetailsButton.text)
        labels.add(workStopButton.text)
        labels.add(newSessionButton.text)
        labels.add(refreshButton.text)
        return labels
    }

    /** Developer-only diagnostics entry point for the screen registry. */
    /** The only registry entry point: More -> Diagnostics -> Developer. */
    internal fun developerRegistryMenuPathForTest(): List<String> = developerRegistryPath

    internal fun registryItemEnabledForTest(): Boolean = developerRegistryItem?.isEnabled == true

    /** The composer's accessible names (never color/shape-only controls). */
    internal fun composerA11yNamesForTest(): List<String> = listOf(
        input.accessibleContext.accessibleName ?: "",
        askModeButton.accessibleContext.accessibleName ?: "",
        taskModeButton.accessibleContext.accessibleName ?: "",
        submitButton.accessibleContext.accessibleName ?: "",
        taskOptionsButton.accessibleContext.accessibleName ?: ""
    )

    /** The shared coarse presentation currently derived from the signals. */
    internal fun presentationForTest(): TaskPresentation =
        taskPresentationFor(currentUxSignalsForTest())

    internal fun currentUxSignalsForTest(): UxSignals = currentUxSignals()

    internal fun workDetailsButtonForTest(): javax.swing.JButton = workDetailsButton

    internal fun workStopButtonForTest(): javax.swing.JButton = workStopButton

    /** The run details moved out of Work into Inspect › Overview. */
    internal fun runDetailsPanelForTest(): JComponent? = runDetailsPanel

    /** The Work center surface that owns the transcript and the anchored composer. */
    internal fun workCenterForTest(): JComponent = workPlane.getComponent(1) as JComponent

    /** The keyboard twin of Run task (Ctrl+Enter in the composer). */
    internal fun submitTaskViaEnterForTest() {
        startTaskFromControls()
    }

    /**
     * Host-journey hook: requests keyboard focus for the ONE composer and
     * reports whether the AWT focus system granted ownership. In a
     * display-less test harness the request is still routed to the composer
     * component; the boolean then honestly reports that no OS window granted
     * ownership.
     */
    internal fun requestComposerFocusForTest(): Boolean {
        input.requestFocusInWindow()
        return input.isFocusOwner
    }

    /**
     * Host-journey hook: types through the composer's own typed-character
     * editor action (the action the Swing keymap dispatches for KEY_TYPED),
     * not by assigning the text, so the journey exercises the insertion path
     * the IDE keystrokes use.
     */
    internal fun typeComposerForTest(text: String) {
        val typed = input.actionMap.get(javax.swing.text.DefaultEditorKit.defaultKeyTypedAction)
            ?: throw IllegalStateException("the composer has no DefaultEditorKit typed-character action")
        for (ch in text) {
            typed.actionPerformed(
                java.awt.event.ActionEvent(
                    input,
                    java.awt.event.ActionEvent.ACTION_PERFORMED,
                    ch.toString()
                )
            )
        }
    }

    /**
     * Host-journey hook: dispatches the exact action bound to Ctrl+Enter in
     * the composer (the same actionMap entry the keystroke resolves to, so no
     * synthetic key event is fabricated).
     */
    internal fun triggerRunTaskActionForTest() {
        val action = input.actionMap.get("faktor.runTask")
            ?: throw IllegalStateException("the composer has no faktor.runTask action")
        action.actionPerformed(
            java.awt.event.ActionEvent(input, java.awt.event.ActionEvent.ACTION_PERFORMED, "")
        )
    }

    internal fun startTaskEnabledForTest(): Boolean = submitButton.isEnabled

    internal fun newSessionEnabledForTest(): Boolean = newSessionButton.isEnabled

    internal fun goalTextForTest(): String = input.text

    internal fun attachmentsCountForTest(): Int = attachments.count()

    internal fun setTaskFieldsForTest(goal: String, criteria: String) {
        input.text = goal
        criteriaField.text = criteria
    }

    /** Fills the Acceptance & contract editors and opens the options sheet. */
    internal fun setContractFieldsForTest(
        constraints: String,
        invariants: String,
        risks: String,
        obligations: String
    ) {
        constraintField.text = constraints
        invariantField.text = invariants
        riskField.text = risks
        obligationField.text = obligations
        if (!taskOptionsSheetOpen()) toggleTaskOptionsSheet()
    }

    /**
     * Selects one finish level by its ordinal; true when it was accepted.
     * A headless test cannot fire the real ActionEvent (the component has no
     * displayable peer), so selection goes through the ButtonGroup model
     * exactly like a click's model step.
     */
    internal fun selectFinishLevelForTest(level: FinishLevel): Boolean {
        val index = level.ordinal
        if (index !in completionLevels.indices) return false
        completionLevels[index].isSelected = true
        return completionLevels[index].isSelected
    }

    /** The visible finish-level label of the one selected level. */
    internal fun selectedFinishLevelLabelForTest(): String = selectedFinishLevel().label()

    /** Opens the Task options sheet exactly like the trigger click. */
    internal fun openTaskOptionsForTest() {
        if (!taskOptionsSheetOpen()) toggleTaskOptionsSheet()
    }

    /** Clicks the sheet's explicit Back exactly like the trigger. */
    internal fun backTaskOptionsForTest() {
        taskOptionsBackButton?.doClick()
    }

    /** Clicks the Task options trigger (opens or closes the sheet). */
    internal fun toggleTaskOptionsForTest(): String {
        taskOptionsButton.doClick()
        return taskOptionsButton.text
    }

    /** Dispatches the exact Escape action bound in the composer/sheet. */
    internal fun triggerEscapeForTest() {
        val escape = javax.swing.KeyStroke.getKeyStroke(java.awt.event.KeyEvent.VK_ESCAPE, 0)
        val key: String = input.inputMap.get(escape) as? String ?: return
        input.actionMap.get(key)?.actionPerformed(
            java.awt.event.ActionEvent(input, java.awt.event.ActionEvent.ACTION_PERFORMED, key)
        )
    }

    /** The bounded sheet surface (host-matrix width/scroll check). */
    internal fun taskOptionsSheetForTest(): JComponent? = optionsSheetPanel

    /** The four editor rows as the capture sees them (smoke assertions). */
    internal fun contractEditorsForTest(): List<String> = listOf(
        constraintField.text,
        invariantField.text,
        riskField.text,
        obligationField.text
    )

    /** The per-run budget editors as the capture sees them. */
    internal fun setBudgetFieldsForTest(tokens: String, cost: String) {
        budgetTokensField.text = tokens
        budgetCostField.text = cost
    }

    internal fun runSummaryTextForTest(): String = runSummaryArea.text

    /** Clicks the run summary strip's Inspect action. */
    internal fun inspectRunForTest() {
        inspectRunsButton.doClick()
    }

    /** True when the Inspect cluster is selected with the Overview view. */
    internal fun overviewSelectedForTest(): Boolean =
        tabs.getTitleAt(tabs.selectedIndex) == "Inspect" &&
            inspectTabs.getTitleAt(inspectTabs.selectedIndex) == "Overview"

    /** The chooser's rendered labels (lists served sessions; "" rows only). */
    internal fun sessionChooserLabelsForTest(): List<String> =
        (0 until sessionChooserModel.size).map { index ->
            val session = sessionChooserModel.getElementAt(index)
            val title = session.title.trim().ifEmpty { "Untitled conversation" }
            bound(title, 40) + " · " + session.state
        }

    internal fun sessionChooserCountForTest(): Int = sessionChooserModel.size

    internal fun sessionChooserOpenEnabledForTest(): Boolean = openSessionButton.isEnabled

    /** Selects one served session in the chooser; false when not served. */
    internal fun selectSessionForTest(sessionId: String): Boolean {
        for (i in 0 until sessionChooserModel.size) {
            if (sessionChooserModel.getElementAt(i).id == sessionId) {
                sessionChooser.selectedIndex = i
                return true
            }
        }
        return false
    }

    /** Clicking Open on the chooser selection (the real open path). */
    internal fun openSelectedSessionForTest() {
        openSessionButton.doClick()
    }

    internal fun currentSessionIdForTest(): String? = service.currentSessionId()

    internal fun attachmentsView(): AttachmentsPanel = attachments

    /** Selects one agent in the Agents combo (the dialog-path smoke hook). */
    internal fun selectAgentForTest(agentId: String): Boolean = agentsPanel.selectAgent(agentId)

    /** Clicks one agent-control button on the EDT (the dialog-path smoke hook). */
    internal fun triggerAgentControlForTest(label: String): Boolean = agentsPanel.clickControl(label)

    /** The rendered transcript (smoke assertions on typed refusals). */
    internal fun transcriptTextForTest(): String = transcriptView.plainText()

    internal fun newSessionForTest() {
        newSessionFromControls(selectedProviderOrDefault(), selectedModelOrDefault())
    }

    /** Runs one refreshAllBlocking cycle on the worker; true when it finished. */
    internal fun refreshNowForTest(timeoutMs: Long = 30_000L): Boolean {
        val latch = CountDownLatch(1)
        worker.execute {
            try {
                refreshAllBlocking()
            } catch (e: Throwable) {
                // The per-panel refreshers record their own unavailable states.
            } finally {
                latch.countDown()
            }
        }
        val finished = latch.await(timeoutMs, TimeUnit.MILLISECONDS)
        // The refreshers queue their panel updates via onEdt (invokeLater from
        // the worker); flush the EDT queue so assertions see the applied state.
        try {
            if (!SwingUtilities.isEventDispatchThread()) {
                SwingUtilities.invokeAndWait { }
            }
        } catch (e: Exception) {
            // A headless/short-lived EDT is not fatal for the caller.
        }
        return finished
    }

    internal fun permissionsView(): PermissionsPanel = permissionsPanel

    /** The bounded Work approval attention card (host-matrix/smoke hook). */
    internal fun workApprovalView(): WorkApprovalCard = workApprovalCard

    /** The wrapping utilities toolbar row (host-matrix/smoke hook). */
    internal fun utilitiesToolbarForTest(): java.awt.Component = utilityToolbar

    /** The shared per-request approval reply gate (smoke hook). */
    internal fun approvalGateForTest(): ApprovalReplyGate = approvalGate

    /** Applies one pending-approval snapshot to BOTH decision surfaces. */
    internal fun updatePendingApprovalsForTest(list: List<NativePermissionEntry>) {
        onEdtValue {
            permissionsPanel.update(list)
            workApprovalCard.update(list)
        }
    }

    /** The once-per-request approval announcements (smoke hook). */
    internal fun approvalAnnouncementsForTest(): List<String> =
        workApprovalCard.announcementsForTest()

    /** Applies one served task view + proof task state to the strip (T22/T12 smoke). */
    /** Test seam: applies a pending-permission snapshot to the state signal. */
    internal fun setPendingPermissionsForTest(list: List<NativePermissionEntry>) {
        onEdtValue {
            pendingPermissions = list
            updateApprovalStatus()
            updateWorkStatus()
        }
    }

    internal fun applyRunSummaryForTest(
        view: NativeTaskView?,
        runStateTag: String? = null,
        proofTaskState: String? = null
    ) {
        onEdtValue {
            currentProofTaskState = proofTaskState
            applyRunSummary(view, runStateTag)
        }
    }

    /** The run-status banner the strip currently renders (null = none). */
    internal fun runStatusBannerForTest(): RunStateView? = onEdtValue { runStatusBanner() }

    internal fun refreshRunSummaryForClientStateForTest() = refreshRunSummaryForClientState()

    /**
     * T06: the ready label + exact guidance copy while the composer holds no
     * draft; null once a draft exists (the ready state is the empty composer).
     */
    internal fun composerReadyForTest(): Pair<String, String>? =
        if (composerReadyRow.isVisible) {
            Pair(composerReadyLabel.fullText, composerHint.fullText)
        } else {
            null
        }

    /**
     * The EXISTING controls the design run-state actions point at (labels as
     * rendered). No design action ever mints a new button.
     */
    internal fun actionControlsForTest(): Map<String, String> = mapOf(
        "runTask" to submitButton.text,
        "stopRun" to cancelRunButton.text,
        "inspect" to inspectRunsButton.text,
        "newSession" to newSessionButton.text
    )

    internal fun prSetupNoticeVisibleForTest(): Boolean = prSetupBody.isVisible

    /** T22 headline / copy / action as rendered (verbatim copy pin). */
    internal fun prSetupCopyForTest(): Triple<String, String, String> =
        Triple(prSetupHeadline.fullText, prSetupCopy.fullText, prSetupButton.text)

    /** The Diagnostics readiness value as the panel renders it. */
    internal fun diagnosticsReadinessForTest(): String = diagnosticsPanel.readinessText()

    /** The last-milestone detail row (raw kind + journal id live here). */
    internal fun diagnosticsMilestoneForTest(): String = diagnosticsPanel.milestoneText()

    internal fun taskTreeView(): TaskTreePanel = treePanel

    internal fun blockersView(): BlockersPanel = blockersPanel

    internal fun diagnosticsView(): DiagnosticsPanel = diagnosticsPanel

    internal fun overviewView(): OverviewPanel = overviewPanel

    internal fun changesView(): ChangesPanel = changesPanel

    internal fun verificationView(): VerificationPanel = verificationPanel

    internal fun terminalView(): TerminalPanel = terminalPanel

    internal fun settingsView(): SettingsPanel = settingsPanel

    internal fun historyView(): HistoryPanel = historyPanel

    internal fun outcomesView(): OutcomesPanel = outcomesPanel

    internal fun tournamentViewForTest(): TournamentPanel = tournamentPanel

    internal fun usageView(): UsagePanel = usagePanel

    internal fun boardView(): BoardPanel = boardPanel

    internal fun evidenceView(): EvidenceNavigatorPanel = navigator

    /**
     * Every reachable view title: the Work/Inspect/History clusters first,
     * then each cluster's views (including nested ones such as the Agents
     * roster/approvals/coordination).
     */
    internal fun tabTitles(): List<String> {
        val out = ArrayList<String>()
        for (i in 0 until tabs.tabCount) {
            out.add(tabs.getTitleAt(i))
            collectTitles(tabs.getComponentAt(i), out)
        }
        return out
    }

    private fun collectTitles(component: java.awt.Component, out: MutableList<String>) {
        if (component is JTabbedPane) {
            for (i in 0 until component.tabCount) {
                out.add(component.getTitleAt(i))
                collectTitles(component.getComponentAt(i), out)
            }
        }
    }

    /** The top-level cluster destinations only (the Work/Inspect/History model). */
    internal fun clusterTitles(): List<String> {
        val out = ArrayList<String>()
        for (i in 0 until tabs.tabCount) out.add(tabs.getTitleAt(i))
        return out
    }

    private fun setControlsEnabled(running: Boolean) {
        startButton.isEnabled = !running
        stopButton.isEnabled = running
        serviceStartItem.isEnabled = !running
        serviceStopItem.isEnabled = running
        refreshButton.isEnabled = running
        abortButton.isEnabled = running
        cancelRunButton.isEnabled = running
        updateSessionChooserState(sessionChooserModel.size)
        setStartControlsEnabled(running)
    }

    /**
     * The start-surface controls (Start task, New session, the completion
     * level, the Task/Ask mode): disabled while a start is in flight, so a
     * pending submission cannot be mutated or duplicated from the UI.
     * Attachment/goal edits are additionally ignored by design: the in-flight
     * request rides the immutable captured draft.
     */
    private fun setStartControlsEnabled(enabled: Boolean) {
        val effective = enabled && service.isRunning() && !startInFlight.get()
        submitButton.isEnabled = effective
        newSessionButton.isEnabled = effective
        for (button in completionLevels) button.isEnabled = effective
        taskModeButton.isEnabled = effective
        askModeButton.isEnabled = effective
        applyIntentPresentation()
    }

    /**
     * A deadline refusal is transport-shaped: the response never arrived, so
     * the request may still have been accepted inside the daemon. Retaining
     * the immutable submission id is what makes the retry idempotent, for
     * both a `transport` failure and a typed `timeout`.
     */
    private fun isTransportFailure(e: NativeApiException): Boolean =
        e.status == 0 && (e.code == "transport" || e.code == "timeout")

    /** S02: the picker is a bounded modal over the served catalog. */
    private fun openEdition5ModelPicker() {
        runAsync("load model catalog") {
            val models = try {
                service.modelCatalog()
            } catch (e: Exception) {
                onEdt {
                    appendSystem(
                        "model catalog unavailable: ${e.message ?: e.javaClass.simpleName}; " +
                            "the picker refuses to invent a model"
                    )
                }
                return@runAsync
            }
            onEdt {
                val chosen = ModelPickerDialog.choose(this@FaktorChatPanel, models)
                if (chosen != null) {
                    val applied = settingsPanel.selectRunModel(chosen.provider, chosen.model)
                    if (applied) {
                        edition5ModelLabel.text = "Model: ${chosen.provider}/${chosen.model}"
                        appendSystem(
                            "run model set to ${chosen.provider}/${chosen.model} for the next task; " +
                                "running calls keep their recorded model"
                        )
                    } else {
                        appendSystem("the selected model is no longer served; no default was changed")
                    }
                }
            }
        }
    }

    /** Attach the session-scoped Edition 5 panels exactly once per session. */
    private fun ensureEdition5SessionPanelsLoaded() {
        val session = service.currentSessionId() ?: return
        if (edition5AttachedFor == session) return
        val client = FaktorConnectionRegistry.client() ?: return
        edition5AttachedFor = session
        edition5ContextPanel.attachClient(client)
        edition5CompactionPanel.attachClient(client)
        edition5CheckpointPanel.attachClient(client)
    }

    /**
     * D-03 Find: pages durable messages backwards (bounded), not just the
     * rendered rows. Reports the exact searched/capped scope.
     */
    private fun searchTranscriptBlocking(query: String) {
        val needle = query.trim()
        if (needle.isEmpty()) return
        val session = service.currentSessionId()
        if (session == null) {
            onEdt { appendSystem("find: no session is open") }
            return
        }
        var before: Long? = null
        var pages = 0
        var scanned = 0
        var hits = 0
        val first = ArrayList<Long>()
        var more = true
        while (more && pages < 50 && scanned < 10_000) {
            val page = try {
                service.messagesBefore(session, before, 200)
            } catch (e: Exception) {
                onEdt {
                    appendSystem("find stopped after $pages page(s): ${e.message ?: e.javaClass.simpleName}")
                }
                return
            }
            pages++
            for (message in page.messages) {
                scanned++
                if (message.text.contains(needle, ignoreCase = true)) {
                    hits++
                    if (first.size < 5) first.add(message.seq)
                }
            }
            more = page.hasMore && page.nextBefore != null && page.nextBefore != before
            before = page.nextBefore
        }
        val scope = if (more) {
            "latest $pages page(s) / $scanned message(s); older pages remain unsearched"
        } else {
            "all $pages durable page(s) / $scanned message(s)"
        }
        onEdt {
            appendSystem(
                if (hits == 0) {
                    "find '$needle': no matches in $scope"
                } else {
                    "find '$needle': $hits match(es) in $scope; first seqs ${first.joinToString(", ")}"
                }
            )
        }
    }

    private fun runAsync(label: String, work: () -> Unit) {
        worker.execute {
            try {
                work()
            } catch (e: Throwable) {
                val message = e.message ?: e.javaClass.simpleName
                onEdt { appendSystem("error: $label: $message") }
            }
        }
    }

    /**
     * One agent control: the observer sees the worker-side dispatch, the
     * control runs on the `faktor-ui` worker, and the roster refreshes after.
     */
    private fun runAgentControl(label: String, work: () -> Unit) {
        runAsync("agent $label") {
            agentControlDispatchObserver?.invoke(label)
            work()
            refreshAgentsBlocking()
        }
    }

    /** The urgent-control twin of [runAsync]: never queued behind refresh work. */
    private fun runControl(label: String, work: () -> Unit) {
        controlWorker.execute {
            try {
                work()
            } catch (e: Throwable) {
                val message = e.message ?: e.javaClass.simpleName
                onEdt { appendSystem("error: $label: $message") }
            }
        }
    }

    /**
     * A direct system line (user action, typed refusal, status): queued and
     * flushed ONCE on the EDT. High-rate stream frames never take this path;
     * they batch through [applyStreamBatch].
     */
    private fun appendSystem(text: String) {
        appendTranscript("[faktor] " + text + "\n")
    }

    /**
     * The optimistic user row: rendered immediately under its logical
     * submission id; the durable message with the same text reconciles into
     * this exact row (see [TranscriptModel.upsertDurable]).
     */
    private fun appendUser(submissionId: String, text: String) {
        onEdt {
            transcriptModel.addProvisional(submissionId, text)?.let { op ->
                if (op is TranscriptRowOp.Upsert) {
                    transcriptSink.upsertRow(
                        op.key.render(), op.payload, op.beforeKey?.render()
                    )
                }
            }
            transcriptSink.flush()
        }
    }

    private fun appendTranscript(text: String) {
        if (SwingUtilities.isEventDispatchThread()) {
            transcriptSink.append(text)
            transcriptSink.flush()
        } else {
            SwingUtilities.invokeLater {
                transcriptSink.append(text)
                transcriptSink.flush()
            }
        }
    }

    private fun onEdt(block: () -> Unit) {
        if (SwingUtilities.isEventDispatchThread()) {
            block()
        } else {
            SwingUtilities.invokeLater { block() }
        }
    }

    /**
     * One Swing READ from a worker: the value is produced on the EDT and
     * returned (a Swing control read off the EDT is a data race). Never call
     * this while already holding a lock the EDT might need.
     */
    private fun <T> onEdtValue(block: () -> T): T {
        if (SwingUtilities.isEventDispatchThread()) return block()
        val holder = java.util.concurrent.atomic.AtomicReference<T>()
        try {
            SwingUtilities.invokeAndWait { holder.set(block()) }
        } catch (e: InterruptedException) {
            Thread.currentThread().interrupt()
            throw IllegalStateException("EDT read interrupted", e)
        } catch (e: Exception) {
            throw IllegalStateException("EDT read failed: ${e.message}", e)
        }
        return holder.get()
    }

    /**
     * Clears every per-session durable view on a session switch: message
     * seqs are per-session (a new session restarts below the previous max),
     * so the transcript model resets with the sink — a stale row map could
     * otherwise reorder or suppress the new session's rows.
     */
    private fun resetSessionView() {
        onEdt {
            // Dispose the old session's scope (queued deltas + offsets) and
            // start a fresh one; a late flush from the previous session can
            // never render into the new view.
            sessionScope.disposeAll()
            sessionScope = SessionUiScope(uiMetrics)
            sessionScope.onDispose { streamPump.discardPending() }
            sessionScope.onDispose { livePump.discardPending() }
            refreshDirty.set(false)
            refreshDelayTimer.stop()
            lastAppliedEventSeq = 0L
            runCatching { service.stopLiveOutput() }
            resetTurnLifecycle()
            transcriptSink.reset()
            transcriptModel.reset()
            boardPanel.reset()
            navigator.resetSessionView()
            currentTree = null
            currentAgents = emptyList()
            overviewPanel.updateModel(null)
            verificationPanel.updateModel(null)
            changesPanel.update(emptyList(), emptyList())
            changesPanel.showConflictFacts(emptyList())
            changesPanel.clearOpenState()
            blockersPanel.update(emptyList(), emptyList())
            permissionsPanel.update(emptyList())
            workApprovalCard.update(emptyList())
            agentsPanel.update(emptyList(), null)
            // The run summary strip is session-owned: a stale run summary must
            // never look like it belongs to the newly opened session.
            currentRunView = null
            currentRunStateTag = null
            currentProofTaskState = null
            runSummaryArea.text = ""
            updateWorkStatus()
            prSetupBody.isVisible = false
            // Session-scoped identities and tracked targets never cross a
            // switch: a stale completion contract or tournament id would be
            // rendered (and acted on) as if it belonged to the new session.
            pendingTaskSubmission = null
            pendingPromptSubmission = null
            submittedCompletion = null
            trackedTournamentId = null
            trackedTournament = null
            tournamentPanel.setTournament(null)
        }
    }
}

/**
 * The parsed per-run budget of the Task options sheet: `tokens` is the
 * exact `max_tokens` (null = the run default), `maxCostMicro` the exact
 * `max_cost_micro` (null = the run default). Both are captured immutably with
 * the rest of the submission.
 */
private data class BudgetCapture(
    val tokens: Long?,
    val maxCostMicro: BigInteger?
)

/**
 * The immutable description of ONE logical task start, captured from the
 * composer at click time. Edits during a pending start can never alter the
 * in-flight request; the captured draft is also what a transport-failure
 * retry re-submits.
 */
private data class TaskStartDraft(
    val goal: String,
    val criteria: List<String>?,
    val mutationMode: String?,
    val files: List<String>,
    val binaries: List<PendingBinaryAttachment>,
    val completionContract: NativeCompletionContract?,
    /** The bounded user-authored engineering-contract rows (null = none). */
    val contractRows: NativeContractRows?,
    /** The per-run budget (null members = the machine/run defaults). */
    val budgetTokens: Long?,
    val budgetMaxCostMicro: BigInteger?
) {
    /** Exact-draft comparison of a retry against the retained submission. */
    fun sameDraftAs(other: TaskStartDraft): Boolean =
        goal == other.goal &&
            criteria == other.criteria &&
            mutationMode == other.mutationMode &&
            files == other.files &&
            completionContract == other.completionContract &&
            contractRows == other.contractRows &&
            budgetTokens == other.budgetTokens &&
            budgetMaxCostMicro == other.budgetMaxCostMicro &&
            binaries.size == other.binaries.size &&
            binaries.indices.all { index ->
                val mine = binaries[index]
                val theirs = other.binaries[index]
                mine.mime == theirs.mime &&
                    mine.filename == theirs.filename &&
                    mine.bytes.contentEquals(theirs.bytes)
            }
}

/** One logical start: the client submission id plus its immutable draft. */
private data class TaskStartSubmission(
    val submissionId: String,
    val draft: TaskStartDraft,
    /** The session the id was minted for; a switch must never reuse it. */
    val sessionId: String?
)

/** Standalone launcher for the Swing panel (the tool-window host entry). */
object FaktorFrontendApp {
    @JvmStatic
    fun main(args: Array<String>) {
        // Machine-scoped resolution only: an explicit CLI path, FAKTOR_BIN or
        // the app-level signed install layout (~/.faktor/install). A
        // repository target/ build is never a fallback.
        val binary = if (args.isNotEmpty()) {
            args[0]
        } else {
            try {
                dev.faktor.backend.DaemonBinary.resolve(
                    null,
                    java.nio.file.Paths.get(System.getProperty("user.home", "."))
                ).executable.toString()
            } catch (e: dev.faktor.backend.DaemonBinary.MissingDaemonException) {
                System.err.println("[faktor] ${e.message}")
                kotlin.system.exitProcess(2)
            }
        }
        val dataDir = if (args.size > 1) {
            args[1]
        } else {
            java.nio.file.Paths.get(
                System.getProperty("java.io.tmpdir"), "faktor-jetbrains"
            ).toString()
        }
        java.nio.file.Files.createDirectories(java.nio.file.Paths.get(dataDir))
        // Headless is honest: a top-level window cannot exist, so refuse
        // loudly instead of constructing a JFrame (HeadlessException). The
        // panel itself is headless-safe; only the standalone window needs a
        // display.
        if (java.awt.GraphicsEnvironment.isHeadless()) {
            System.err.println(
                "[faktor] the standalone launcher needs a display; run the panel inside " +
                    "the IDE tool window when headless"
            )
            kotlin.system.exitProcess(2)
        }
        val service = FaktorFrontendService(
            java.nio.file.Paths.get(binary),
            java.nio.file.Paths.get(dataDir)
        )
        // The standalone launcher's session workspace is its working
        // directory (the daemon child inherits the same cwd); relative
        // attachments resolve against it.
        val workspaceRoot = java.nio.file.Paths.get(System.getProperty("user.dir"))
            .toAbsolutePath().normalize()
        SwingUtilities.invokeLater {
            val frame = JFrame("Faktor")
            val panel = FaktorChatPanel(service, workspaceRoot = workspaceRoot)
            frame.contentPane.add(panel)
            frame.defaultCloseOperation = WindowConstants.DO_NOTHING_ON_CLOSE
            frame.addWindowListener(object : java.awt.event.WindowAdapter() {
                override fun windowClosing(e: java.awt.event.WindowEvent) {
                    panel.shutdown()
                    val stopper = Thread {
                        service.stop()
                        SwingUtilities.invokeLater {
                            frame.isVisible = false
                            frame.dispose()
                            System.exit(0)
                        }
                    }
                    stopper.isDaemon = true
                    stopper.start()
                }
            })
            frame.pack()
            frame.setSize(1280, 820)
            frame.setLocationRelativeTo(null)
            frame.isVisible = true
            panel.startDaemon()
        }
    }
}

/**
 * The human-readable transcript line of one durable stream milestone. The
 * raw wire kind and journal id never render here: they land in the
 * Diagnostics details surface ([DiagnosticsPanel.setStreamMilestone]).
 * Unmapped kinds degrade to a calm generic status — wire identifiers are
 * never echoed into ordinary user-visible copy.
 */
internal fun milestoneCopy(kind: String): String = when (kind) {
    "turn_completed" -> "Turn completed"
    "session_ended" -> "Conversation ended"
    "suspended" -> "Streaming paused"
    "resumed" -> "Streaming resumed"
    "failed" -> "The run failed"
    "permission_granted" -> "Permission granted"
    "permission_denied" -> "Permission denied"
    "permission_expired" -> "The permission request expired"
    "subagent_completed" -> "A delegated task finished"
    "tool_cancelled" -> "A tool call was cancelled"
    "context_compacted" -> "Older context was compacted"
    "compact_rejected" -> "Context compaction was refused"
    "replay_started" -> "Reconnecting…"
    "crash_detected" -> "Recovered from an interruption"
    "recovery_applied" -> "Recovery applied"
    else -> "Live update"
}

/**
 * The human transcript line of one typed auto-start refusal (the tool-window
 * lifecycle hook); the machine kind stays in the Diagnostics readiness
 * detail. Every refusal names the explicit "Start service" recovery.
 */
internal fun autoStartRefusalCopy(kind: String, detail: String): String = when (kind) {
    "binary_missing" ->
        "The Faktor service is not installed on this machine; " +
            "use Start service to locate the daemon."
    "outside_trusted_install" ->
        "The configured daemon is outside the trusted Faktor install, " +
            "so it was not started automatically; use Start service to review and start it."
    "trust_unverified" ->
        "The daemon has no recorded checksum, so it was not started automatically; " +
            "use Start service to confirm it."
    "trust_refused" ->
        "The daemon failed its checksum verification and was not started; " +
            "use Start service to review the install."
    "startup_failed" -> "The service did not start: $detail"
    else -> "The service was not started automatically: $detail"
}

/**
 * The stable machine code of the daemon's explicit configuration blocker
 * (`crates/orchestrator/src/completion_steps.rs:NATIVE_PR_SCM_NOT_CONFIGURED`)
 * recorded when a task's completion contract requests the native PR step but
 * no canonical SCM provider is wired. It is the ONLY signal the T22 surface
 * accepts: no other PR failure is ever read as a missing-SCM configuration.
 */
internal const val NATIVE_PR_SCM_NOT_CONFIGURED = "native_pr_scm_not_configured"

/**
 * True when the requested PR finish step is blocked by the daemon's typed
 * SCM-not-configured code. A missing read (or any other failure) stays
 * false — PR success is never inferred and a cause is never fabricated.
 */
internal fun prScmSetupBlocked(view: NativeTaskView?): Boolean {
    val steps = view?.completion?.steps ?: return false
    return steps.any { step ->
        step.step.equals("pr", ignoreCase = true) &&
            step.detail.contains(NATIVE_PR_SCM_NOT_CONFIGURED)
    }
}

/** One immutable logical prompt held across a retryable failure. */
internal data class PromptSubmission(
    val submissionId: String,
    val sessionId: String,
    val text: String
)

/**
 * Selects the logical prompt to send: the retained submission is reused ONLY
 * for the byte-identical draft in the same session (a lost-response retry
 * replays the original receipt); anything else mints a fresh id.
 */
internal fun selectPromptSubmission(
    retained: PromptSubmission?,
    sessionId: String,
    text: String,
    newId: () -> String
): PromptSubmission =
    retained?.takeIf { it.sessionId == sessionId && it.text == text }
        ?: PromptSubmission(newId(), sessionId, text)
