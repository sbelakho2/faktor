// The Faktor VS Code product surface over the native daemon:
//
//   - daemon.ts        spawns and owns the faktor-cli process
//   - nativeClient.ts  strict typed fetch client of the daemon's /native/* routes
//   - eventStream.ts   SSE journal stream with cursor resume
//   - state.ts         the observable snapshot store + transcript reducer
//   - webview.ts       the chat panel (strict CSP, nonce, no remote code)
//
// Commands: start/stop the daemon, open the chat, start a task, cancel the
// active run, reply to a pending permission. The status bar mirrors daemon +
// task state. Every daemon string that reaches the UI passes through the
// strict native validators first; nothing is rendered from an unvalidated
// response.

import * as vscode from 'vscode';
import { readFileSync, writeFileSync } from 'node:fs';
import { basename, resolve } from 'node:path';
import { createHash, randomUUID } from 'node:crypto';

/** The attachment-envelope correlation id: the host mints one when the
 *  webview did not supply an id, so the webview is never an id authority. */
function hostMessageId(raw: unknown): string {
  if (typeof raw === 'string' && raw.length > 0 && raw.length <= 64) {
    return raw;
  }
  return randomUUID();
}
import {
  DaemonHandle,
  DaemonSettingScopes,
  applyMachineScopedWrite,
  binaryTrust,
  installRootFor,
  machineScopedSetting,
  resolveDaemon,
  startDaemon,
  stopDaemon,
  workspaceScopedSettingRefusal,
} from './daemon';
import {
  PreflightReadiness,
  PreflightView,
  derivePreflight,
} from './preflight';
import {
  ServiceAuthority,
  ServiceConnection,
  ServiceLaunchPlan,
  ServiceLifecycle,
  ServiceLifecycleView,
  ServicePreparation,
  ServiceStartRequest,
  servicePlanAuthority,
  validateServiceConfiguration,
} from './serviceLifecycle';
import { awaitReady } from './readiness';
import { LifecycleFence } from './lifecycleFence';
import {
  CoalescingRefreshScheduler,
  nodeSchedulerTimers,
} from './refreshScheduler';
import {
  FetchLike,
  NativeApiError,
  NativeTransportError,
  NativeBillingOverview,
  NativeBillingUsage,
  NativeClient,
  NativeCompletionContract,
  NativeEntitlementSnapshot,
  NativeEvidenceSelector,
  NativeIdentity,
  NativeIndexCoverageSnapshot,
  NativeMessagePage,
  NativeModelInfo,
  NativePermissionEntry,
  NativeProjection,
  NativeSessionUsage,
  NativeSessionSummary,
  NativeTaskRun,
  NativeTaskProof,
  NativeTaskVerification,
  NativeTaskView,
  NativeTournament,
  NativeTournamentSummary,
  NativeVerificationView,
  ResponseLike,
  classifyPermissionReplyFailure,
  permissionReplyFailureMessage,
} from './nativeClient';
import {
  EventStream,
  EventStreamStatus,
  LiveChunkFrame,
  LiveOutputMessage,
  LiveOutputStream,
  LiveOutputStreamStatus,
  SseFetchLike,
  SseResponseLike,
  fetchLiveSnapshot,
} from './eventStream';
import {
  TurnEvent,
  TurnLifecycle,
  emptyTurnLifecycle,
  turnDisplayPhase,
  turnLifecycleIsLive,
  turnReducer,
  turnStopOpId,
} from './turnState';
import {
  type ProvenanceView,
  mergeTranscriptPages,
  BoardStateSummary,
  FaktorStore,
  Json,
  RunSummary,
  TaskCompletionSummary,
  TaskSummary,
  TranscriptEntry,
  UsageSummary,
  VerificationSummary,
  activeRunIdAfter,
  boardStateFromPage,
  cancelRunTarget,
  nextPixelPresence,
  parseBoardPostRequest,
  parseBoardReadRequest,
  permissionSummaries,
  summarizeAgents,
  transcriptContentEqual,
  transcriptFromMessages,
  transcriptSignature,
  unavailableBoardState,
} from './state';
import { CockpitTaskVerification, CockpitUsagePanel, buildUsagePanel, projectCockpit, tournamentViewOf } from './cockpit';
import { loadConversation } from './askFlow.ts';
import {
  classifyAskFailure,
  sendAskTurnWithRecovery,
  type AskRecoveryConnection,
} from './askRecovery.ts';
import { composerIntentOf } from './uxState.ts';
import { refreshReadPlan } from './refreshReads.ts';
import { Edition8Client } from './edition8/client.ts';
import type { Edition8PanelDeps } from './edition8/panel.ts';
import { pickModel, registerEdition8 } from './edition8/commands.ts';
import { registerLspDiagnostics } from './lspDiagnostics.ts';
import { addContextDraft, toggleFavorite } from './edition8/hostDraft.ts';
import type { PixelPresence } from './pixelAgents';
import {
  microBalance,
  microFromNumber,
  microText,
  microToString,
  microUsdText,
  MICRO_I64_MAX,
  sumMicro,
  type MicroMoney,
} from './money.ts';
import {
  AdmitFailure,
  AttachmentAdmissionPolicy,
  CachedEvidenceView,
  ComposerAttachmentStore,
  EMERGENCY_ATTACHMENT_POLICY,
  EvidenceAuthorityCache,
  MAX_COMPOSER_ATTACHMENTS,
  MAX_PENDING_ATTACHMENT_BYTES,
  PendingSubmission,
  PendingSubmissionRetainer,
  StartSubmissionSnapshot,
  StartTaskSettings,
  TaskStartGate,
  admitPendingSubmission,
  attachmentPolicyForModel,
  boundedWebviewFiles,
  composerAttachmentRefusal,
  composerMimeForFilename,
  completionContractFromPicks,
  hasCompletionSteps,
  parseCompletionContract,
  parseContractRows,
  parsePendingSubmission,
  parseRunBudget,
  resolveRunBudget,
  submissionRetryable,
  verificationAuthorityKey,
} from './taskStart';
import {
  buildChangesSummary,
  candidateDisplayOf,
  changeRowsOf,
  evidenceDiffTitle,
  evidenceDocumentUri,
  resolveReviewBasis,
  sourceDiffEvidenceIdOf,
  type ChangeRowSummary,
} from './changeReview';
import { registerEvidenceDocumentProvider } from './evidenceDocuments';
import {
  OUTCOMES_PAGE_LIMIT,
  buildOutcomesView,
  mergeOutcomesPage,
  outcomesOrganizationForRead,
  parseOutcomesReadRequest,
  unavailableOutcomesView,
  valueEntitlementsRefusalStatusLine,
  valueEntitlementsStatusLine,
} from './outcomes';
import {
  SessionBindings,
  boundSessionFor,
  canonicalWorkspaceKey,
  pruneBindings,
  withBinding,
} from './workspaceBinding';
import { sessionSwitchDecision } from './sessionSwitch';
import { MAX_STEER_CHARS, normalizeSteerText } from './steer';
import {
  attachmentRefusalProjection,
  projectError,
  type UserErrorKind,
  type UserErrorProjection,
} from './errors';
import {
  ControlPlaneAuthSession,
  ControlPlaneResolution,
  ControlPlaneScope,
  controlPlaneAuthSessionValid,
  controlPlaneEndpointValid,
  controlPlaneScopeValid,
  controlPlaneTokenForClient,
  logoutControlPlane,
  resolveControlPlaneToken,
  storeControlPlaneToken,
  watchControlPlaneSecretChanges,
} from './controlPlaneAuth';
import { ChatMessage, ChatViewProvider } from './webview';

const HISTORY_PAGE_LIMIT = 100;
const MAX_EVIDENCE_PREVIEW_BYTES = 256 * 1024;
const SESSION_BINDINGS_KEY = 'faktor.sessionBindings';
/** One bounded newest-first board page per read (the daemon caps at 100). */
const BOARD_PAGE_LIMIT = 100;
/** One bounded billing usage page per read (the daemon caps at 200). */
const BILLING_PAGE_LIMIT = 50;
/**
 * The control-plane endpoint has NO default: cloud control-plane calls stay
 * disabled until the operator explicitly configures an endpoint (machine
 * scope; see the Faktor settings docs). The coordinates are non-secret.
 */
const DEFAULT_CONTROL_PLANE_ENDPOINT = '';
/** The last control-plane notice, so a repeated refusal never spams the chat. */
let lastControlPlaneNotice: string | null = null;
/** The last workspace-scope refusal, deduped like the control-plane notice. */
let lastMachineSettingNotice: string | null = null;

/**
 * One in-flight Ask submission of ONE conversation (audit item 7). The
 * receipt/error that eventually lands is correlated by all three fields:
 * the lifecycle generation captured at send time, the conversation the
 * prompt was admitted on, and the logical submission id (mirrored by the
 * panel's own submission token in `startResult`).
 */
interface AskInFlightEntry {
  readonly generation: number;
  /** The visible-conversation epoch captured at send time. */
  readonly viewEpoch: number;
  /** The conversation key this submission belongs to. */
  readonly conversationKey: string;
  /** The resolved session id while still on the same binding (null = unresolved). */
  sessionId: string | null;
  readonly submissionId: string;
  /** The panel's submission token echoed back in `startResult`. */
  readonly panelToken: string | null;
  readonly goal: string;
}

interface ActiveSession {
  daemon: DaemonHandle | null;
  client: NativeClient | null;
  stream: EventStream | null;
  /** The session-scoped live model-output subscription (`/live/output`). */
  liveStream: LiveOutputStream | null;
  sessionId: string | null;
  activeRunId: string | null;
  /** TRUE after an Ask turn until a Task starts or the user switches
   * conversations: stream-driven refreshes stay conversational and never
   * read task/proof/billing state. */
  askOnly: boolean;
  /**
   * Ask single-flight PER CONVERSATION (audit item 7): one plain prompt at
   * a time per conversation key (the bound session id, or `workspace`
   * before the first resolve). Each entry captures the view generation,
   * the resolved conversation and the submission id/panel token the
   * eventual receipt is routed back with — a reply that finishes after the
   * operator switched conversations can never retarget the visible one.
   */
  askInFlight: Map<string, AskInFlightEntry>;
  /**
   * Paged history (audit item 13): which session the accumulated
   * transcript belongs to, whether the daemon serves OLDER history, and
   * the `before` cursor for the next scrollback page. A switch resets all
   * three so one conversation's accumulation never leaks into another.
   */
  transcriptSessionId: string | null;
  transcriptHasMore: boolean;
  transcriptNextBefore: number | null;
  loadingOlder: boolean;
  /**
   * The operator's explicit per-conversation model choice (audit item 13):
   * applied to subsequent Task runs of THIS conversation through the
   * daemon's per-run `model` override; a switch drops it (it is never a
   * silent global default).
   */
  futureModelOverride: string | null;
  /**
   * The daemon's protocol-capability handshake (audit item 17): null when
   * the serving daemon does not advertise it (the Diagnostics surface then
   * reports the coherent degradation instead of claiming the features).
   */
  protocolCapabilities: {
    readonly liveStreaming: boolean | null;
    readonly attachments: boolean | null;
    readonly turnState: boolean | null;
  } | null;
  refreshing: boolean;
  refreshTimer: NodeJS.Timeout | null;
  /** Reuse the last task-verification read while its authority key is
   * unchanged; a key change or a failed fetch drops the cached view and
   * records an explicit unavailable — stale evidence is never handed back. */
  taskVerificationCache: EvidenceAuthorityCache<NativeTaskVerification>;
  /** Reuse the last strict proof read while its inputs are unchanged. */
  taskProofKey: string | null;
  taskProof: NativeTaskProof | null;
  /** Explicit refusal of the last proof read (never a silent stale proof). */
  taskProofUnavailable: string | null;
  /** The tournament the cockpit auto-loads (tracked id, else newest). */
  tournamentId: string | null;
  /** Persistent deterministic pixel presence per ChildId. */
  pixelPresence: Map<string, PixelPresence>;
  /** The completion contract the operator submitted for the active task
   * (kept in host memory so the cockpit can tell a contracted run from a
   * plain prompt even when the daemon serves no completion read). */
  completionContract: NativeCompletionContract | null;
  /** Board read watermark: posts newer than this revision are unread. Only
   * an explicit read/post moves it; automatic refreshes never mark read. */
  boardSeenRevision: number;
  /** Commercial-metering read state: the identity's organization fixes the
   * billing tenant; the cursor stack drives the panel's paging controls. */
  billingIdentity: NativeIdentity | null;
  billingEntitlements: NativeEntitlementSnapshot | null;
  billingUsage: NativeBillingUsage | null;
  billingOverview: NativeBillingOverview | null;
  billingCursor: string | null;
  billingPrevCursors: string[];
}

const active: ActiveSession = {
  daemon: null,
  client: null,
  stream: null,
  liveStream: null,
  sessionId: null,
  activeRunId: null,
  askOnly: false,
  askInFlight: new Map(),
  transcriptSessionId: null,
  transcriptHasMore: false,
  transcriptNextBefore: null,
  loadingOlder: false,
  futureModelOverride: null,
  protocolCapabilities: null,
  refreshing: false,
  refreshTimer: null,
  taskVerificationCache: new EvidenceAuthorityCache<NativeTaskVerification>(),
  taskProofKey: null,
  taskProof: null,
  taskProofUnavailable: null,
  tournamentId: null,
  pixelPresence: new Map(),
  completionContract: null,
  boardSeenRevision: 0,
  billingIdentity: null,
  billingEntitlements: null,
  billingUsage: null,
  billingOverview: null,
  billingCursor: null,
  billingPrevCursors: [],
};

const store = new FaktorStore();
let chatProvider: ChatViewProvider | null = null;

/**
 * Stream frames coalesce into bounded refreshes. `refresh()` itself is
 * single-flight; a frame that arrives while a refresh is running asks the
 * scheduler for the next window instead of being dropped.
 */
let streamRefresh: CoalescingRefreshScheduler | null = null;
streamRefresh = new CoalescingRefreshScheduler((done) => {
  if (active.refreshing) {
    streamRefresh?.request();
    done();
    return;
  }
  void refresh().finally(done);
}, nodeSchedulerTimers);
let statusBar: vscode.StatusBarItem | null = null;
/** The activation context, so deactivate-time paths can still publish. */
let activeContext: vscode.ExtensionContext | null = null;
/**
 * Lifecycle generation fence (audit P1-VSCODE): bumped whenever the daemon
 * authority changes (start/stop) or the active session id changes. `refresh()`
 * captures it once and refuses to patch ANY host state after a transition, so
 * a slow read for a dead daemon/session can never overwrite a newer snapshot.
 * Bumping also disarms the in-flight refresh flag: the transition itself owns
 * the state, and the next `refresh()` re-arms it.
 */
const lifecycle = new LifecycleFence();
function bumpLifecycle(): void {
  lifecycle.bump();
  active.refreshing = false;
}
// Bounded local pending state for submission retries (audit 29): uploaded
// attachment ids survive a failed start and a retry resolves them first,
// uploading only the attachments still absent.
const pendingSubmissionRetainer = new PendingSubmissionRetainer();
// Host single-flight of task starts (audit 32): one in-flight logical
// submission per host; a lost-response retry reuses its immutable snapshot
// and submission id so the daemon returns the original receipt.
const taskStartGate = new TaskStartGate();
// Bounded HOST-SIDE composer attachment state. The webview holds metadata
// ids only; picker-selected bytes are read here (never through the webview)
// and every sendGoal resolves the ids into an immutable snapshot. The store
// survives a failed start (retry reuses the durable uploads) and is cleared
// ONLY by a durable acceptance, an explicit remove/clear, or a session
// switch.
const composerAttachments = new ComposerAttachmentStore();
let composerAttachmentsSessionId: string | null = null;

// ---------------------------------------------- preflight + service authority

/**
 * Host state behind the single preflight/readiness presentation (I06): the
 * last machine-scoped resolution + trust verdict, the readiness outcome and
 * the last non-cancel failure detail. `publishPreflight` derives the exact
 * view from this plus the live daemon status and the workspace presence, so
 * the Work empty-state card and the Diagnostics mirror can never disagree.
 */
const preflightHost = {
  startingAttempt: false,
  resolution: null as ReturnType<typeof resolveDaemon> | null,
  trust: null as ReturnType<typeof binaryTrust> | null,
  resolveError: null as string | null,
  readiness: 'idle' as PreflightReadiness,
  readinessDetail: null as string | null,
  failureDetail: null as string | null,
};

/**
 * The supervised service connection: the native client is created only after
 * the daemon claimed the exact health identity, and readiness has passed.
 */
interface HostServiceConnection extends ServiceConnection {
  readonly daemon: DaemonHandle;
  readonly client: NativeClient;
}

/**
 * The ONE service lifecycle. Null before activation; every command, the
 * auto-open path and the Diagnostics Start/Stop buttons use this same
 * instance, so concurrent triggers share one launch.
 */
let serviceLifecycle: ServiceLifecycle<HostServiceConnection> | null = null;
/** Journal position to resume after a crash; never reset by a manual stop. */
let resumeCursor = 0;
/**
 * The VISIBLE-conversation epoch (audit item 7): bumped only by an
 * explicit conversation switch (the one operation that retargets the
 * panel's conversation). An Ask flight captures it at send time; its
 * receipt/error is treated as stale the moment the operator switches —
 * while a daemon-side session rebind inside the SAME conversation never
 * moves it.
 */
let conversationViewEpoch = 0;
/** Dedupe authority notices: the same cause interrupts once, not per tick. */
let lastServiceAuthorityKey: string | null = null;

// ------------------------------------------------------------------ helpers

function config<T>(key: string, fallback: T): T {
  return vscode.workspace.getConfiguration('faktor').get<T>(key, fallback);
}

/**
 * A MACHINE-SCOPED setting read (daemon inputs and the non-secret optional
 * control-plane coordinates): the global (user/machine) value is returned and
 * any workspace/workspace-folder value is ignored and surfaced once as a
 * typed refusal. The settings are declared `"scope": "machine"` in
 * package.json; this inspect-based gate is the ONE defence-in-depth adapter
 * that also reports a repo-supplied value instead of silently ignoring it.
 */
function machineSetting<T>(key: string, fallback: T): T {
  const inspected = vscode.workspace.getConfiguration('faktor').inspect<T>(key);
  const outcome = machineScopedSetting(key, inspected ?? undefined, fallback);
  if (outcome.refusal !== null && outcome.refusal.message !== lastMachineSettingNotice) {
    lastMachineSettingNotice = outcome.refusal.message;
    chatProvider?.postNotice('error', outcome.refusal.message);
  }
  return outcome.value;
}

function workspaceRoot(context: vscode.ExtensionContext): string {
  const folder = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (folder) {
    return folder;
  }
  // apps/vscode -> repository root.
  return resolve(context.extensionUri.fsPath, '..', '..');
}

/**
 * Resolve + trust-verify the machine-scoped executable for the preflight
 * card. Never consults the workspace (resolveDaemon has no target/ lookup);
 * a missing daemon becomes the typed T02 state, never a fabricated path.
 */
function refreshPreflightResolution(context: vscode.ExtensionContext): void {
  const binaryPath = machineSetting('binaryPath', '');
  const dataDir = machineSetting('dataDir', '');
  const installRoot = machineSetting('installRoot', '');
  try {
    const resolved = resolveDaemon({
      workspaceRoot: workspaceRoot(context),
      binaryPath: binaryPath.length > 0 ? binaryPath : undefined,
      dataDir: dataDir.length > 0 ? dataDir : undefined,
      installRoot: installRoot.length > 0 ? installRoot : undefined,
    });
    preflightHost.resolution = resolved;
    preflightHost.trust = binaryTrust(resolved);
    preflightHost.resolveError = null;
  } catch (error) {
    preflightHost.resolution = null;
    preflightHost.trust = null;
    preflightHost.resolveError = messageOf(error);
  }
}

/**
 * Derive and publish the ONE Diagnostics preflight view. The normal surface
 * does NOT render this matrix: it renders only a service authority row (or
 * the transient connection notice), so healthy/starting service vocabulary
 * stays inside Diagnostics.
 */
function publishPreflight(context: vscode.ExtensionContext | null = activeContext): void {
  const snapshot = store.snapshot();
  const keepTrustAuthority =
    snapshot.serviceAuthority?.kind === 'trust_changed' ||
    snapshot.serviceAuthority?.kind === 'trust_unverified';
  if (
    context !== null &&
    !preflightHost.startingAttempt &&
    snapshot.daemon !== 'running' &&
    !keepTrustAuthority
  ) {
    refreshPreflightResolution(context);
  }
  const view: PreflightView = derivePreflight({
    workspaceOpen: (vscode.workspace.workspaceFolders?.length ?? 0) > 0,
    daemon: snapshot.daemon,
    startingAttempt: preflightHost.startingAttempt,
    resolution: preflightHost.resolution,
    trust: preflightHost.trust,
    resolveError: preflightHost.resolveError,
    readiness: preflightHost.readiness,
    readinessDetail: preflightHost.readinessDetail,
    failureDetail: preflightHost.failureDetail,
  });
  store.patch({ preflight: view });
}

/**
 * The ONE service preparation path: validate machine-scoped settings, resolve
 * the executable without consulting the workspace, compute its trust and
 * refuse anything that is not a verified installed executable. Every start
 * (auto, restart, explicit) passes through this gate before a spawn.
 */
function prepareService(
  context: vscode.ExtensionContext,
  mode: 'auto' | 'restart' | 'explicit',
  request: ServiceStartRequest,
): Promise<ServicePreparation> | ServicePreparation {
  if ((vscode.workspace.workspaceFolders?.length ?? 0) === 0) {
    return {
      ok: false,
      failure: {
        kind: 'workspace_unavailable',
        detail: 'this window has no project folder; no service was started against the extension directory',
      },
    };
  }
  const binaryPath = machineSetting('binaryPath', '');
  const dataDir = machineSetting('dataDir', '');
  const installRoot = machineSetting('installRoot', '');
  const extraArgs = machineSetting<string[]>('extraArgs', []);
  const startupTimeoutMs = machineSetting('startupTimeoutMs', 45_000);
  const invalid = validateServiceConfiguration({
    binaryPath,
    dataDir,
    installRoot,
    startupTimeoutMs,
    extraArgs,
  });
  if (invalid !== null) {
    return { ok: false, failure: { kind: 'configuration_invalid', detail: invalid } };
  }
  const root = workspaceRoot(context);
  let resolved: ReturnType<typeof resolveDaemon>;
  try {
    resolved = resolveDaemon({
      workspaceRoot: root,
      binaryPath: binaryPath.length > 0 ? binaryPath : undefined,
      dataDir: dataDir.length > 0 ? dataDir : undefined,
      installRoot: installRoot.length > 0 ? installRoot : undefined,
    });
  } catch (error) {
    preflightHost.resolution = null;
    preflightHost.trust = null;
    preflightHost.resolveError = messageOf(error);
    return { ok: false, failure: { kind: 'binary_missing', detail: messageOf(error) } };
  }
  const trust = binaryTrust(resolved);
  preflightHost.resolution = resolved;
  preflightHost.trust = trust;
  preflightHost.resolveError = null;
  const plan: ServiceLaunchPlan = {
    executable: resolved.executable,
    source: resolved.source,
    trust,
    installRoot: installRootFor({
      workspaceRoot: root,
      dataDir: dataDir.length > 0 ? dataDir : undefined,
      installRoot: installRoot.length > 0 ? installRoot : undefined,
    }),
    dataDir: dataDir.length > 0 ? dataDir : undefined,
    extraArgs,
    startupTimeoutMs,
  };
  let allowUnverified = request.allowUnverified;
  if (trust.state === 'unverified' && mode === 'explicit' && !allowUnverified) {
    // Explicit Diagnostics start may confirm the exact machine-scoped path;
    // automatic starts never do.
    const choice = vscode.window.showWarningMessage(
      `Faktor: unverified service executable ${resolved.executable}`,
      {
        modal: true,
        detail: `${trust.detail}\n\nStart this exact executable anyway?`,
      },
      'Start anyway',
    );
    return Promise.resolve(choice).then((picked) => {
      if (picked !== 'Start anyway') {
        return {
          ok: false as const,
          failure: {
            kind: 'trust_unverified' as const,
            detail: 'the unverified executable was not confirmed; nothing was started',
          },
        };
      }
      allowUnverified = true;
      return finishServicePreparation(context, plan, mode, allowUnverified);
    });
  }
  return finishServicePreparation(context, plan, mode, allowUnverified);
}

function finishServicePreparation(
  context: vscode.ExtensionContext,
  plan: ServiceLaunchPlan,
  mode: 'auto' | 'restart' | 'explicit',
  allowUnverified: boolean,
): ServicePreparation {
  const authority = servicePlanAuthority({
    plan,
    mode,
    workspaceTrusted: vscode.workspace.isTrusted,
    workspaceRoot: workspaceRoot(context),
    allowUnverified,
  });
  if (authority !== null) {
    return { ok: false, failure: { kind: authority.kind, detail: authority.detail } };
  }
  return { ok: true, plan };
}

/**
 * The spawn + identity + readiness half. The supervisor owns WHEN this runs;
 * this function owns exactly which executable and connection is accepted.
 */
async function launchService(
  context: vscode.ExtensionContext,
  plan: ServiceLaunchPlan,
  signal: AbortSignal,
): Promise<HostServiceConnection> {
  const daemon = await startDaemon({
    workspaceRoot: workspaceRoot(context),
    // The already-verified path is passed explicitly so the spawn cannot
    // re-resolve to a different file between the gate and the launch.
    binaryPath: plan.executable,
    dataDir: plan.dataDir,
    installRoot: plan.installRoot,
    extraArgs: plan.extraArgs,
    startupTimeoutMs: plan.startupTimeoutMs,
    signal,
  });
  const client = new NativeClient({
    baseUrl: daemon.baseUrl,
    bearerToken: daemon.bearerToken,
    fetch: fetchAdapter(),
  });
  try {
    await applyControlPlaneCredential(client, context);
    const health = await client.health();
    const readiness = await awaitReady(client, {
      timeoutMs: plan.startupTimeoutMs,
      signal,
    });
    if (readiness.state !== 'ready') {
      throw new Error(readiness.detail);
    }
    active.daemon = daemon;
    active.client = client;
    bumpLifecycle();
    // Item 17: the additive protocol-capability handshake is stored on
    // connect; null (older daemon) is reported as NOT advertised.
    active.protocolCapabilities = health.protocol;
    store.patch({
      daemon: 'running',
      daemonDetail: health.version,
      baseUrl: daemon.baseUrl,
      streamStatus: 'connecting',
    });
    // A service (re)connect clears ONLY a service/stream-scoped error; a
    // turn failure recorded earlier stays on its own surface (item 11).
    clearServiceScopedError();
    refreshProvenance(context, health.version);
    return {
      daemon,
      client,
      port: daemon.port,
      pid: daemon.pid,
      baseUrl: daemon.baseUrl,
      version: health.version,
      alive: () => daemon.alive(),
    };
  } catch (error) {
    await stopDaemon(daemon);
    throw error;
  }
}

/** Reconcile/rebind/resume after every successful connection (once). */
async function resumeService(
  context: vscode.ExtensionContext,
  connection: HostServiceConnection,
): Promise<void> {
  const cursor = resumeCursor;
  resumeCursor = 0;
  await ensureSession(connection.client, context);
  startStream(cursor);
  scheduleRefresh(0);
  publishPreflight(context);
}

/** Drop a supervised connection; crash keeps the durable session binding. */
async function shutdownService(
  context: vscode.ExtensionContext,
  connection: HostServiceConnection,
  reason: 'crash' | 'stop' | 'dispose',
): Promise<void> {
  const current = active.daemon === connection.daemon;
  if (current) {
    if (reason === 'crash') {
      resumeCursor = active.stream?.cursor ?? resumeCursor;
      active.stream?.stop();
      active.stream = null;
      // The turn lifecycle is KEPT across a crash: turn records are
      // durable, and the rebound session's subscription re-drives the same
      // entries with the new server-instance id (a restart never fakes a
      // completion from a dropped stream).
      stopLiveOutput();
      active.daemon = null;
      active.client = null;
      // The durable workspace binding rebinds the SAME conversation after
      // restart; clearing the in-memory id forces the listing/binding path.
      active.sessionId = null;
      active.activeRunId = null;
      bumpLifecycle();
      store.patch({
        daemon: 'starting',
        daemonDetail: 'reconnecting',
        connectionNotice: 'reconnecting',
        streamStatus: 'stopped',
        busy: false,
      });
    } else {
      resetHostState(context);
    }
  }
  await stopDaemon(connection.daemon);
}

/**
 * Publish the supervisor view into the ONE snapshot. The normal surface only
 * reads `connectionNotice`/`serviceAuthority`; every service detail lands in
 * the Diagnostics mirror.
 */
function publishServiceView(context: vscode.ExtensionContext, view: ServiceLifecycleView): void {
  const phase = view.phase;
  preflightHost.startingAttempt = phase === 'connecting' || phase === 'reconnecting';
  preflightHost.readiness =
    phase === 'running'
      ? 'ready'
      : phase === 'connecting' || phase === 'reconnecting'
        ? 'waiting'
        : 'idle';
  preflightHost.readinessDetail = view.diagnostics.readinessDetail;
  preflightHost.failureDetail = view.diagnostics.lastFailure;
  if (view.authority !== null && view.authority.kind === 'trust_changed') {
    preflightHost.trust = {
      state: 'refused',
      source: preflightHost.trust?.source ?? 'install-layout',
      detail: view.authority.detail,
    };
  } else if (view.authority !== null && view.authority.kind === 'trust_unverified') {
    preflightHost.trust = {
      state: 'unverified',
      source: preflightHost.trust?.source ?? 'install-layout',
      detail: view.authority.detail,
    };
  }
  if (phase === 'running') {
    lastServiceAuthorityKey = null;
  }
  store.patch({
    daemon:
      phase === 'running'
        ? 'running'
        : phase === 'connecting' || phase === 'reconnecting'
          ? 'starting'
          : phase === 'blocked'
            ? 'error'
            : 'stopped',
    daemonDetail:
      view.diagnostics.baseUrl ??
      view.diagnostics.lastFailure ??
      '',
    baseUrl: view.diagnostics.baseUrl,
    connectionNotice: view.connectionNotice,
    serviceAuthority: view.authority,
    service: view.diagnostics,
    lastError: view.diagnostics.lastFailure,
  });
  publishPreflight(context);
}

function authorityErrorKind(kind: ServiceAuthority['kind']): UserErrorKind {
  switch (kind) {
    case 'binary_missing':
      return 'service_binary_missing';
    case 'trust_changed':
      return 'service_trust_changed';
    case 'trust_unverified':
      return 'service_trust_unverified';
    case 'startup_budget_exhausted':
      return 'service_budget_exhausted';
    case 'configuration_invalid':
      return 'service_configuration_invalid';
    case 'outside_trusted_install':
      return 'service_untrusted_install';
    default:
      return 'unexpected';
  }
}

/** Only genuine authority reaches the native chrome; deduped per cause. */
function handleServiceAuthority(authority: ServiceAuthority): void {
  const key = `${authority.kind}:${authority.detail}`;
  if (key === lastServiceAuthorityKey) {
    return;
  }
  lastServiceAuthorityKey = key;
  if (authority.kind === 'workspace_untrusted' || authority.kind === 'workspace_unavailable') {
    chatProvider?.postNotice('info', authority.copy);
    return;
  }
  if (authority.kind === 'user_action_required') {
    chatProvider?.postNotice('error', authority.copy);
    return;
  }
  recordError(
    projectError({
      kind: authorityErrorKind(authority.kind),
      message: authority.detail,
      code: authority.kind,
    }),
    true,
  );
}

function ensureServiceLifecycle(
  context: vscode.ExtensionContext,
): ServiceLifecycle<HostServiceConnection> {
  if (serviceLifecycle !== null) {
    return serviceLifecycle;
  }
  serviceLifecycle = new ServiceLifecycle<HostServiceConnection>({
    prepare: (mode, request) => prepareService(context, mode, request),
    launch: (plan, signal) => launchService(context, plan, signal),
    probe: (connection) => connection.daemon.alive(),
    shutdown: (connection, reason) => shutdownService(context, connection, reason),
    resume: (connection) => resumeService(context, connection),
    publish: (view) => publishServiceView(context, view),
    onAuthority: (authority) => handleServiceAuthority(authority),
  });
  return serviceLifecycle;
}


function canonicalWorkspace(context: vscode.ExtensionContext): {
  key: string | null;
  fsPath: string | undefined;
} {
  const folder = vscode.workspace.workspaceFolders?.[0];
  if (folder) {
    return { key: canonicalWorkspaceKey(folder.uri.toString()), fsPath: folder.uri.fsPath };
  }
  return { key: canonicalWorkspaceKey(context.extensionUri.toString()), fsPath: undefined };
}

function workspaceTitle(): string {
  const folder = vscode.workspace.workspaceFolders?.[0];
  return folder ? `Faktor · ${folder.name}` : 'Faktor';
}

function readBindings(context: vscode.ExtensionContext): SessionBindings {
  const stored = context.workspaceState.get<SessionBindings>(SESSION_BINDINGS_KEY);
  if (stored === undefined || stored === null || typeof stored !== 'object') {
    return {};
  }
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(stored)) {
    if (typeof value === 'string' && key.length > 0 && key.length <= 2048) {
      out[key] = value;
    }
  }
  return out;
}

async function writeBindings(
  context: vscode.ExtensionContext,
  bindings: SessionBindings,
): Promise<void> {
  await context.workspaceState.update(SESSION_BINDINGS_KEY, bindings);
}

function fetchAdapter(): FetchLike {
  return (url, init) => fetch(url, init) as unknown as Promise<ResponseLike>;
}

function sseAdapter(): SseFetchLike {
  return (url, init) => fetch(url, init) as unknown as Promise<SseResponseLike>;
}

function messageOf(error: unknown): string {
  const raw = error instanceof Error ? error.message : String(error);
  return raw.length > 500 ? `${raw.slice(0, 500)}…` : raw;
}

function errorCodeOf(error: unknown): string | null {
  return error instanceof NativeApiError ? error.code : null;
}

/**
 * Patch the bounded raw error (Diagnostics) and post ONE structured
 * projection to the panel. `toast` mirrors the previous `reportError`
 * behavior for foreground gestures; background reads stay silent in the IDE
 * chrome and only surface inside the panel.
 */
function recordError(projection: UserErrorProjection, toast = false): void {
  store.patch({ lastError: projection.technical, lastErrorKind: projection.kind });
  chatProvider?.postUserError(projection);
  if (toast) {
    void vscode.window.showErrorMessage(`Faktor: ${projection.summary}`);
  }
}

function projectOf(error: unknown, kind: UserErrorKind = 'unexpected'): UserErrorProjection {
  let resolved = kind;
  if (resolved === 'unexpected') {
    const cause = classifyAskFailure(error);
    resolved =
      cause === 'transport'
        ? 'daemon_unreachable'
        : cause === 'stale_session'
          ? 'conversation_missing'
          : resolved;
  }
  const code = error instanceof NativeTransportError ? error.code : errorCodeOf(error);
  return projectError({ kind: resolved, message: messageOf(error), code });
}

/** A background read failure: panel-visible, no IDE toast (no user gesture). */
function recordBackgroundError(error: unknown, kind: UserErrorKind = 'unexpected'): void {
  recordError(projectOf(error, kind));
}

/**
 * Service/stream-scoped error kinds (audit item 11): a SERVICE recovery
 * (reconnect, restart, adoption) may clear exactly these. A turn-scoped
 * failure (`run_failure`, `conversation_missing`) and a webview-local
 * refusal (`attachment_refused`, `permission_expired`, `unexpected`) are
 * NEVER cleared by service recovery — they belong to their own surface.
 */
const SERVICE_SCOPED_ERROR_KINDS: ReadonlySet<string> = new Set([
  'daemon_down',
  'daemon_unreachable',
  'daemon_start_failed',
  'service_not_ready',
  'service_binary_missing',
  'service_trust_changed',
  'service_trust_unverified',
  'service_budget_exhausted',
  'service_configuration_invalid',
  'service_untrusted_install',
  'stream_gap',
]);

/** One bounded provenance asset hash (item 17); null = unreadable file. */
function provenanceHash(root: string, relPath: string): string | null {
  try {
    const bytes = readFileSync(resolve(root, relPath));
    if (bytes.length > 8 * 1024 * 1024) {
      return null;
    }
    return createHash('sha256').update(bytes).digest('hex');
  } catch {
    return null;
  }
}

/**
 * The Diagnostics provenance surface (audit item 17): the packaged build
 * commit + asset content hashes (from `build-info.json`, written by
 * scripts/package-vsix.mjs) compared against the LIVE installed bytes, plus
 * the daemon's version/executable and the protocol handshake. Missing or
 * unreadable stamps report honestly ("uncommitted build" / per-asset
 * mismatch) — never a fabricated match.
 */
function refreshProvenance(
  context: vscode.ExtensionContext,
  daemonVersion: string | null = null,
): void {
  try {
    const root = context.extensionUri.fsPath;
    const manifest = JSON.parse(
      readFileSync(resolve(root, 'package.json'), 'utf8'),
    ) as { version?: unknown };
    let stamp: {
      commit?: unknown;
      packagedAt?: unknown;
      assets?: Record<string, unknown>;
    } | null = null;
    try {
      stamp = JSON.parse(readFileSync(resolve(root, 'build-info.json'), 'utf8'));
    } catch {
      stamp = null;
    }
    const packaged = stamp !== null && typeof stamp.assets === 'object' && stamp.assets !== null
      ? (stamp.assets as Record<string, unknown>)
      : {};
    const assets: Array<{
      path: string;
      packagedSha256: string;
      liveSha256: string;
      matches: boolean;
    }> = [];
    const keys = Object.keys(packaged).sort().slice(0, 40);
    for (const rel of keys) {
      const want = typeof packaged[rel] === 'string' ? (packaged[rel] as string) : '';
      const live = provenanceHash(root, rel);
      assets.push({
        path: rel,
        packagedSha256: want,
        liveSha256: live ?? '',
        matches: live !== null && live === want,
      });
    }
    const service = store.snapshot().service;
    const view: ProvenanceView = {
      extensionVersion: typeof manifest.version === 'string' ? manifest.version : 'unknown',
      buildCommit: typeof stamp?.commit === 'string' ? (stamp.commit as string) : null,
      packagedAt: typeof stamp?.packagedAt === 'string' ? (stamp.packagedAt as string) : null,
      assets,
      assetsMatch: assets.length > 0 && assets.every((asset) => asset.matches),
      daemonVersion: daemonVersion ?? (store.snapshot().daemonDetail || null),
      daemonExecutable: service?.executable ?? null,
      protocol: active.protocolCapabilities,
    };
    store.patch({ provenance: view });
  } catch (error) {
    recordBackgroundError(error);
  }
}

/**
 * Clear `lastError` ONLY while it carries a service/stream-scoped kind (or
 * no recorded kind at all). A service recovery never clears a turn failure.
 */
function clearServiceScopedError(): void {
  const kind = store.snapshot().lastErrorKind;
  if (kind === null || SERVICE_SCOPED_ERROR_KINDS.has(kind)) {
    store.patch({ lastError: null, lastErrorKind: null });
  }
}

function reportError(error: unknown, kind: UserErrorKind = 'unexpected'): void {
  const projection = projectOf(error, kind);
  if (store.snapshot().daemon === 'starting') {
    store.patch({ daemon: 'error', daemonDetail: projection.technical });
  }
  recordError(projection, true);
}

// ------------------------------------------------- control-plane credentials
//
// The cloud control-plane credential lives in `vscode.SecretStorage` keyed by
// the (non-secret) endpoint + organization coordinates. The deprecated
// plaintext `faktor.controlToken` setting is read once for a migration prompt
// and then deleted; its value is NEVER handed to the native client (the
// daemon-password `bearerToken` path is separate and unchanged).

/** One input-box refusal: never echoes a credential-bearing endpoint. */
function controlPlaneEndpointInputError(value: string): string | null {
  return controlPlaneEndpointValid(value)
    ? null
    : 'Enter an http(s) endpoint without embedded credentials, query or fragment.';
}

function controlPlaneNotice(level: 'info' | 'error', text: string): void {
  if (lastControlPlaneNotice === text) {
    return;
  }
  lastControlPlaneNotice = text;
  chatProvider?.postNotice(level, text);
}

function controlPlaneScope(): ControlPlaneScope | null {
  const scope: ControlPlaneScope = {
    endpoint: machineSetting('controlPlaneEndpoint', ''),
    organization: machineSetting('controlPlaneOrganization', ''),
  };
  return controlPlaneScopeValid(scope) ? scope : null;
}

async function persistControlPlaneScope(scope: ControlPlaneScope): Promise<void> {
  const cfg = vscode.workspace.getConfiguration('faktor');
  await cfg.update('controlPlaneEndpoint', scope.endpoint, vscode.ConfigurationTarget.Global);
  await cfg.update(
    'controlPlaneOrganization',
    scope.organization,
    vscode.ConfigurationTarget.Global,
  );
}

/**
 * The NON-secret auth-session coordinates of the stored credential. The
 * logout route's strict body must name the `{organization, session_id}` the
 * presented token owns; an opaque token cannot be reverse-mapped, so the id
 * is provisioned with the credential (SSO callback / bootstrap result) and
 * only ever used for the revoke call.
 */
function controlPlaneAuthSession(): ControlPlaneAuthSession | null {
  const scope = controlPlaneScope();
  if (scope === null) {
    return null;
  }
  const session: ControlPlaneAuthSession = {
    organization: scope.organization,
    sessionId: machineSetting('controlPlaneAuthSession', ''),
  };
  return controlPlaneAuthSessionValid(session) ? session : null;
}

async function persistControlPlaneAuthSession(sessionId: string): Promise<void> {
  await vscode.workspace
    .getConfiguration('faktor')
    .update('controlPlaneAuthSession', sessionId, vscode.ConfigurationTarget.Global);
}

/** The deprecated plaintext setting, behind the migration seam. */
function legacyPlaintextSetting(): {
  read(): string | null;
  clear(): Promise<void>;
} {
  return {
    read: () => {
      const raw = machineSetting('controlToken', '');
      return typeof raw === 'string' && raw.length > 0 ? raw : null;
    },
    clear: async () => {
      const cfg = vscode.workspace.getConfiguration('faktor');
      const inspected = cfg.inspect<string>('controlToken');
      if (inspected?.globalValue !== undefined) {
        await cfg.update('controlToken', undefined, vscode.ConfigurationTarget.Global);
      }
      if (inspected?.workspaceValue !== undefined) {
        await cfg.update('controlToken', undefined, vscode.ConfigurationTarget.Workspace);
      }
    },
  };
}

/**
 * Resolve the (endpoint, organization) coordinates when only the legacy
 * plaintext credential exists: prompt for them before the migration so the
 * secret can be keyed, and persist them (non-secret) for the next run.
 */
async function ensureControlPlaneScope(hasLegacy: boolean): Promise<ControlPlaneScope | null> {
  const current = controlPlaneScope();
  if (current !== null) {
    return current;
  }
  if (!hasLegacy) {
    return null;
  }
  const endpoint = await vscode.window.showInputBox({
    title: 'Faktor: migrate control-plane credential',
    prompt:
      'Cloud control-plane endpoint the credential belongs to (required; there is no default endpoint)',
    value: machineSetting('controlPlaneEndpoint', ''),
    ignoreFocusOut: true,
    validateInput: controlPlaneEndpointInputError,
  });
  if (endpoint === undefined || endpoint.trim().length === 0) {
    return null;
  }
  const organization = await vscode.window.showInputBox({
    title: 'Faktor: migrate control-plane credential',
    prompt: 'Organization (or account) id the credential belongs to',
    value: machineSetting('controlPlaneOrganization', ''),
    ignoreFocusOut: true,
  });
  if (organization === undefined || organization.trim().length === 0) {
    return null;
  }
  const scope: ControlPlaneScope = {
    endpoint: endpoint.trim(),
    organization: organization.trim(),
  };
  await persistControlPlaneScope(scope);
  return scope;
}

/**
 * Resolve the control-plane credential for the running client: secret store
 * first, one-shot migration of the deprecated plaintext setting, refusal
 * (never a send) when the migration is declined or impossible.
 */
async function applyControlPlaneCredential(
  client: NativeClient,
  context: vscode.ExtensionContext,
): Promise<void> {
  const plaintext = legacyPlaintextSetting();
  // The deprecated plaintext value is read ONCE for this resolution; it is
  // never retained, never sent, and deleted by a successful migration.
  const legacyToken = plaintext.read();
  const scope = await ensureControlPlaneScope(legacyToken !== null);
  let resolution: ControlPlaneResolution;
  try {
    resolution = await resolveControlPlaneToken({
      secrets: context.secrets,
      plaintext,
      scope,
      legacyToken,
      confirmMigration: async () => {
        const choice = await vscode.window.showWarningMessage(
          'Faktor: the control-plane credential is stored in plaintext settings.',
          {
            modal: true,
            detail:
              'Move it into the OS/IDE secret store now? On migration the plaintext setting is deleted; ' +
              'until then the value is refused for cloud calls and never sent.',
          },
          'Migrate now',
        );
        return choice === 'Migrate now';
      },
    });
  } catch (error) {
    client.setControlToken(null);
    controlPlaneNotice('error', `control-plane credential unavailable: ${messageOf(error)}`);
    return;
  }
  if (resolution.kind === 'legacy-refused') {
    client.setControlToken(null);
    controlPlaneNotice('error', resolution.reason);
    return;
  }
  if (resolution.kind === 'migrated') {
    controlPlaneNotice('info', 'control-plane credential migrated into the OS/IDE secret store');
  } else if (resolution.kind === 'secret' && resolution.legacyCleared) {
    controlPlaneNotice('info', 'deprecated plaintext control-plane credential deleted (secret store wins)');
  } else {
    lastControlPlaneNotice = null;
  }
  client.setControlToken(controlPlaneTokenForClient(resolution));
}

/** `Faktor: Sign in to Control Plane` — store the credential securely. */
async function controlPlaneSignIn(context: vscode.ExtensionContext): Promise<void> {
  const current = controlPlaneScope();
  const endpoint = await vscode.window.showInputBox({
    title: 'Faktor: control-plane sign-in',
    prompt:
      'Cloud control-plane endpoint (required; there is no default endpoint; non-secret and keys the ' +
      'stored credential)',
    value: current?.endpoint ?? DEFAULT_CONTROL_PLANE_ENDPOINT,
    ignoreFocusOut: true,
    validateInput: controlPlaneEndpointInputError,
  });
  if (endpoint === undefined || endpoint.trim().length === 0) {
    return;
  }
  const organization = await vscode.window.showInputBox({
    title: 'Faktor: control-plane sign-in',
    prompt: 'Organization (or account) id',
    value: current?.organization ?? '',
    ignoreFocusOut: true,
  });
  if (organization === undefined || organization.trim().length === 0) {
    return;
  }
  const token = await vscode.window.showInputBox({
    title: 'Faktor: control-plane sign-in',
    prompt: 'Credential (stored in the OS/IDE secret store, never in settings)',
    password: true,
    ignoreFocusOut: true,
  });
  if (token === undefined || token.trim().length === 0) {
    controlPlaneNotice('error', 'no credential entered; nothing was stored');
    return;
  }
  const sessionId = await vscode.window.showInputBox({
    title: 'Faktor: control-plane sign-in',
    prompt:
      'Auth-session id (non-secret; from the SSO callback or the bootstrap result). Needed so sign-out can revoke the ' +
      'server-side session; leave empty when unknown.',
    value: machineSetting('controlPlaneAuthSession', ''),
    ignoreFocusOut: true,
  });
  if (sessionId === undefined) {
    controlPlaneNotice('error', 'sign-in cancelled; nothing was stored');
    return;
  }
  const scope: ControlPlaneScope = {
    endpoint: endpoint.trim(),
    organization: organization.trim(),
  };
  try {
    await storeControlPlaneToken(context.secrets, legacyPlaintextSetting(), scope, token);
    await persistControlPlaneScope(scope);
    await persistControlPlaneAuthSession(sessionId.trim());
    active.client?.setControlToken(token);
    lastControlPlaneNotice = null;
    controlPlaneNotice(
      'info',
      `control-plane credential for ${scope.organization} stored in the OS/IDE secret store`,
    );
    await refresh();
  } catch (error) {
    reportError(error);
  }
}

/**
 * `Faktor: Sign out of Control Plane` — revoke the remote session when the
 * daemon is reachable AND the non-secret session id is known, then delete the
 * local secret unconditionally. A refusal is reported as the daemon typed it
 * (401 not-owner, 404 unknown session, 409 control plane disabled); a 404 is
 * the route's own not-found, never a "daemon predates the route" story.
 */
async function controlPlaneSignOut(context: vscode.ExtensionContext): Promise<void> {
  const result = await logoutControlPlane({
    secrets: context.secrets,
    plaintext: legacyPlaintextSetting(),
    scope: controlPlaneScope(),
    session: controlPlaneAuthSession(),
    revoke: async (_token, session) => {
      const client = active.client;
      if (!client) {
        throw new Error('the daemon is not running, so the control plane is unreachable');
      }
      try {
        await client.revokeControlSession(session.organization, session.sessionId);
      } catch (error) {
        if (error instanceof NativeApiError) {
          if (error.status === 401) {
            throw new Error(
              `the stored credential does not own the named auth session, or the session is already revoked ` +
                `(HTTP 401 ${error.code}); no remote session was revoked`,
            );
          }
          if (error.status === 404) {
            throw new Error(
              `the daemon found no auth session ${session.sessionId} under ${session.organization} for this ` +
                `credential (HTTP 404 ${error.code}); no remote session was revoked`,
            );
          }
          if (error.status === 409) {
            throw new Error(
              `the daemon control plane is disabled (HTTP 409 ${error.code}); no remote session was revoked`,
            );
          }
        }
        throw error;
      }
    },
  });
  if (result.rotatedDuringLogout) {
    // The stored credential rotated while the revoke was in flight: the
    // revoke targeted the CAPTURED token, and the NEW secret was left
    // untouched (compare-and-delete). Keep the new credential live instead
    // of clearing it and reporting a sign-out that never happened.
    controlPlaneNotice(
      'error',
      'signed out: the stored credential was rotated by another window during sign-out, so the NEW credential ' +
        'was left in the OS/IDE secret store and remains active (the captured token was the one revoked)',
    );
    if (active.client) {
      await applyControlPlaneCredential(active.client, context);
    }
    await refresh();
    return;
  }
  active.client?.setControlToken(null);
  // Commercial data is lazy and must not survive a sign-out.
  store.patch({ usagePanel: null, accountStatus: 'not_connected' });
  // The session id is dead with the session (and useless without a stored
  // credential); clear the non-secret coordinate so a stale id is never
  // presented for a future credential.
  await persistControlPlaneAuthSession('');
  const remote =
    result.remoteError !== null
      ? `the remote revoke failed (${result.remoteError}); the local credential was still deleted`
      : result.hadSecret
        ? 'the remote session was revoked'
        : 'no stored credential was found';
  controlPlaneNotice(result.remoteError === null ? 'info' : 'error', `signed out: ${remote}`);
  await refresh();
}

/**
 * The completion-contract block of the cockpit/task card. Step rows come
 * from the daemon when it serves them; otherwise the block is derived from
 * the DURABLE task-run state and the submitted contract:
 *   - a non-terminal run shows every requested step as `pending`;
 *   - a `Done` (VerifiedComplete) run shows `succeeded` — the durable
 *     completion gate refuses `VerifiedComplete` until every requested
 *     step row is Succeeded, so certification is the step proof;
 *   - a terminal Failed/Cancelled run shows `unknown` with the explicit
 *     reason that this daemon exposes no per-step completion read. A
 *     missing read is never presented as success.
 */
function completionSummaryOf(
  view: NativeTaskView,
  runState: string | null,
  submitted: NativeCompletionContract | null,
): TaskCompletionSummary | null {
  if (view.completion !== null) {
    return {
      includeCommit: view.completion.contract.include_commit,
      includePush: view.completion.contract.include_push,
      includePr: view.completion.contract.include_pr,
      steps: view.completion.steps.map((row) => ({
        step: row.step,
        status: row.status,
        detail: row.detail.length > 0 ? row.detail : null,
      })),
      source: 'daemon',
      reason: null,
    };
  }
  if (!hasCompletionSteps(submitted)) {
    return null;
  }
  const contract = submitted as NativeCompletionContract;
  const requested: Array<{ step: string; enabled: boolean }> = [
    { step: 'commit', enabled: contract.include_commit },
    { step: 'push', enabled: contract.include_push },
    { step: 'pr', enabled: contract.include_pr },
  ];
  const selected = requested.filter((entry) => entry.enabled);
  const state = (runState ?? view.state).trim().toLowerCase();
  if (state === 'done') {
    return {
      includeCommit: contract.include_commit,
      includePush: contract.include_push,
      includePr: contract.include_pr,
      steps: selected.map((entry) => ({
        step: entry.step,
        status: 'succeeded',
        detail: 'certified by the durable completion gate (all requested steps succeeded)',
      })),
      source: 'derived',
      reason: null,
    };
  }
  if (state === 'failed' || state === 'cancelled') {
    return {
      includeCommit: contract.include_commit,
      includePush: contract.include_push,
      includePr: contract.include_pr,
      steps: selected.map((entry) => ({
        step: entry.step,
        status: 'unknown',
        detail: 'the run ended before certification',
      })),
      source: 'unavailable',
      reason: 'the serving daemon exposes no per-step completion read; exact statuses are not served',
    };
  }
  return {
    includeCommit: contract.include_commit,
    includePush: contract.include_push,
    includePr: contract.include_pr,
    steps: selected.map((entry) => ({
      step: entry.step,
      status: 'pending',
      detail: 'awaiting deterministic verification and the durable completion gate',
    })),
    source: 'derived',
    reason: null,
  };
}

// ------------------------------------------------------------ daemon + session

/**
 * Diagnostics → Service "Start service" (and any legacy panel start action).
 * Normal users never see this: the same supervisor auto-starts on open.
 */
async function startServer(
  context: vscode.ExtensionContext,
  options: { readonly confirmUnverified?: boolean } = {},
): Promise<void> {
  lastServiceAuthorityKey = null;
  await ensureServiceLifecycle(context).startExplicit(options.confirmUnverified === true);
  if (active.client && !active.sessionId) {
    // Already-running service: resume the bound conversation for callers that
    // arrived before the supervisor's resume callback completed.
    await ensureSession(active.client, context);
    startStream();
  }
  // An explicit start gesture that RESOLVES means the service answers now:
  // any earlier "could not be reached" card must drop with the next
  // snapshot. launchService clears lastError on a fresh launch; the
  // already-running path (in-memory handle reuse or a cross-host
  // serve.lock adoption) reaches here without it, and without this clear
  // the ONE actionable card outlives its own condition forever.
  if (active.client && store.snapshot().lastError !== null) {
    clearServiceScopedError();
  }
}

/** Abort one in-flight start; auto-restart stays off until an explicit start. */
function cancelStart(): void {
  if (serviceLifecycle === null) {
    return;
  }
  serviceLifecycle.cancel();
  chatProvider?.postNotice('info', 'service start cancelled; nothing is running');
}

/** T03 `Cancel`: keep the service stopped and dismiss the confirmation. */
function dismissUnverifiedStart(context: vscode.ExtensionContext): void {
  cancelStart();
  preflightHost.failureDetail = null;
  publishPreflight(context);
  chatProvider?.postNotice(
    'info',
    'start cancelled: the resolved executable was not confirmed; nothing was executed',
  );
}

/**
 * `Locate daemon…`: pick the machine-scoped executable through a native
 * open-file dialog and write `faktor.binaryPath` through the ONE
 * machine-scoped write adapter (global scope, exactly one write). A
 * workspace/workspace-folder effective value refuses typed (D11) and nothing
 * is written; Cancel changes nothing. A workspace value is NEVER resolved or
 * executed.
 */
async function locateDaemon(context: vscode.ExtensionContext): Promise<void> {
  const cfg = vscode.workspace.getConfiguration('faktor');
  const inspectScopes = (): DaemonSettingScopes<string> => {
    const inspected = cfg.inspect<string>('binaryPath');
    return inspected ?? {};
  };
  const refusalBefore = workspaceScopedSettingRefusal('faktor.binaryPath', inspectScopes());
  if (refusalBefore !== null) {
    lastMachineSettingNotice = refusalBefore.message;
    chatProvider?.postNotice('error', refusalBefore.message);
    void vscode.window.showErrorMessage(`Faktor: ${refusalBefore.message}`);
    return;
  }
  const selection = await vscode.window.showOpenDialog({
    canSelectMany: false,
    title: 'Faktor: Locate daemon…',
    openLabel: 'Use executable',
  });
  if (!selection || selection.length === 0) {
    return;
  }
  const target = selection[0]!;
  let isFile = false;
  try {
    isFile = (await vscode.workspace.fs.stat(target)).type === vscode.FileType.File;
  } catch {
    isFile = false;
  }
  if (!isFile) {
    chatProvider?.postNotice('error', `${target.fsPath} is not a file; nothing was written`);
    return;
  }
  // Re-inspect after the dialog: a value that became workspace-scoped while
  // the dialog was open must refuse too (never a write the effective
  // configuration would ignore).
  const result = await applyMachineScopedWrite(
    'faktor.binaryPath',
    inspectScopes(),
    target.fsPath,
    (key, value) =>
      Promise.resolve(cfg.update(key, value, vscode.ConfigurationTarget.Global)),
  );
  if (result.refusal !== null) {
    lastMachineSettingNotice = result.refusal.message;
    chatProvider?.postNotice('error', result.refusal.message);
    void vscode.window.showErrorMessage(`Faktor: ${result.refusal.message}`);
    return;
  }
  chatProvider?.postNotice(
    'info',
    `faktor.binaryPath now points at ${target.fsPath} (machine/user scope)`,
  );
  const authority = store.snapshot().serviceAuthority;
  if (authority !== null && authority.kind !== 'startup_budget_exhausted') {
    // Choosing the executable is the user action the authority row asked
    // for; re-run the same strict gate instead of making them find Start.
    void serviceLifecycle?.startExplicit();
  } else {
    publishPreflight(context);
  }
}

/** Diagnostics → Service "Stop service": explicit, sticky until Start. */
function stopServer(context: vscode.ExtensionContext | null = activeContext): void {
  if (serviceLifecycle === null) {
    resetHostState(context);
    return;
  }
  void serviceLifecycle.stopExplicit();
}

/** Clear every host/session surface without touching the owned process. */
function resetHostState(context: vscode.ExtensionContext | null = activeContext): void {
  // Any in-flight refresh captured the previous generation: fence it out and
  // disarm the flag before the state reset below.
  bumpLifecycle();
  resumeCursor = 0;
  streamRefresh?.reset();
  if (active.refreshTimer !== null) {
    clearTimeout(active.refreshTimer);
    active.refreshTimer = null;
  }
  preflightHost.startingAttempt = false;
  preflightHost.readiness = 'idle';
  preflightHost.readinessDetail = null;
  preflightHost.failureDetail = null;
  active.stream?.stop();
  active.stream = null;
  stopLiveOutput();
  resetTurnLifecycle();
  active.daemon = null;
  active.client = null;
  active.sessionId = null;
  active.activeRunId = null;
  active.refreshing = false;
  active.taskVerificationCache.clear();
  active.taskProofKey = null;
  active.taskProof = null;
  active.taskProofUnavailable = null;
  active.tournamentId = null;
  active.pixelPresence = new Map();
  active.completionContract = null;
  active.askOnly = false;
  active.boardSeenRevision = 0;
  active.billingIdentity = null;
  active.billingEntitlements = null;
  active.billingUsage = null;
  active.billingOverview = null;
  active.billingCursor = null;
  active.billingPrevCursors = [];
  taskStartGate.reset();
  composerAttachments.clear();
  composerAttachmentsSessionId = null;
  chatProvider?.postAttachmentsCleared();
  chatProvider?.postStreamRecovery(false);
  chatProvider?.postStreamBlocked(null);
  store.patch({
    daemon: 'stopped',
    daemonDetail: '',
    baseUrl: null,
    session: null,
    machineState: 'unknown',
    machineLabel: 'Daemon stopped',
    sessions: [],
    sessionsUnavailable: null,
    runs: [],
    activeRunId: null,
    proofTaskState: null,
    agents: [],
    permissions: [],
    task: null,
    verification: null,
    usage: null,
    cockpit: null,
    cockpitSections: [],
    tournament: null,
    transcript: [],
    streamStatus: 'stopped',
    connectionNotice: null,
    serviceAuthority: null,
    service: null,
    lastError: null,
    lastErrorKind: null,
    busy: false,
  });
  publishPreflight(context);
}

/**
 * Create a session bound to the exact canonical workspace and persist the
 * workspace binding. This is the ONE creation path for a new conversation;
 * it never switches the active session by itself (callers decide whether the
 * new row becomes active).
 */
async function createBoundSession(
  client: NativeClient,
  context: vscode.ExtensionContext,
  provider: string,
  model: string,
): Promise<{ readonly id: string; readonly title: string; readonly created_ms: number }> {
  const workspace = canonicalWorkspace(context);
  const created = await client.createSession({
    provider,
    model,
    workspace: workspace.fsPath,
    title: workspaceTitle(),
  });
  const bindings = withBinding(readBindings(context), workspace.key, created.id);
  await writeBindings(context, bindings);
  store.patch({
    session: {
      id: created.id,
      title: created.title,
      provider,
      model,
      state: 'unknown',
      createdMs: created.created_ms,
    },
  });
  return created;
}

async function ensureSession(
  client: NativeClient,
  context: vscode.ExtensionContext,
  options: { readonly force?: boolean } = {},
): Promise<string> {
  const force = options.force === true;
  if (active.sessionId && !force) {
    return active.sessionId;
  }
  if (force) {
    // The cached id was just proven missing (a typed 404 from the daemon) or
    // belongs to a previous service: fence in-flight work and re-resolve the
    // workspace binding from the listing instead of reusing it.
    active.sessionId = null;
    bumpLifecycle();
  }
  const previousSessionId = active.sessionId;
  const workspace = canonicalWorkspace(context);
  let sessions: Awaited<ReturnType<NativeClient['listSessions']>> = [];
  let listed = false;
  try {
    sessions = await client.listSessions();
    listed = true;
    store.patch({ sessions, sessionsUnavailable: null });
  } catch (error) {
    // The listing is its own read (the History surface's source): a refusal
    // is recorded as the explicit unavailable state, never as a stale list.
    store.patch({ sessionsUnavailable: messageOf(error) });
    recordBackgroundError(error);
  }
  // Bind against the EXACT canonical workspace identity — never sessions[0].
  let bindings = pruneBindings(readBindings(context), sessions);
  if (listed) {
    await writeBindings(context, bindings);
  }
  const boundId = boundSessionFor(workspace.key, sessions, bindings);
  const boundSummary = boundId !== null ? sessions.find((entry) => entry.id === boundId) : undefined;
  if (boundId !== null && boundSummary !== undefined) {
    active.sessionId = boundId;
    bumpLifecycle();
    store.patch({ session: boundSummary });
  } else if (
    !force &&
    boundId === null &&
    !listed &&
    workspace.key !== null &&
    bindings[workspace.key]
  ) {
    // The listing failed; trust the durable binding for this exact
    // workspace rather than minting a duplicate session. A forced rebind
    // never takes this path: the old id was already proven missing.
    active.sessionId = bindings[workspace.key] as string;
    bumpLifecycle();
  } else {
    let provider = config('defaultProvider', '');
    let model = config('defaultModel', '');
    if (provider.length === 0 || model.length === 0) {
      try {
        const catalog = await client.modelCatalog();
        if (catalog.length > 0) {
          provider = provider || catalog[0]!.provider;
          model = model || catalog[0]!.model;
        }
      } catch {
        // The catalog is optional; session creation below still applies.
      }
    }
    const created = await createBoundSession(
      client,
      context,
      provider || 'faktor',
      model || 'default',
    );
    active.sessionId = created.id;
    bumpLifecycle();
  }
  const sessionId = active.sessionId;
  if (sessionId !== previousSessionId) {
    // The board watermark belongs to ONE session/run family.
    active.boardSeenRevision = 0;
  }
  if (composerAttachmentsSessionId !== null && composerAttachmentsSessionId !== sessionId) {
    // Uploads are bound to ONE session; the visible set and its pending
    // submission identity cannot leak into a different session.
    composerAttachments.clear();
    chatProvider?.postAttachmentsCleared();
  }
  composerAttachmentsSessionId = sessionId;
  try {
    const page = await client.messages(sessionId, { limit: HISTORY_PAGE_LIMIT });
    const transcript = transcriptOf(page);
    store.patch({ transcript, transcriptRevision: transcriptSignature(transcript) });
  } catch (error) {
    recordBackgroundError(error);
  }
  return sessionId;
}

/**
 * The ONE explicit conversation switch (a History row's Open action and the
 * `faktor.switchSession` command). The pure `sessionSwitchDecision` decides
 * first: a blank or already-active request is a NO-OP that never stops or
 * re-starts the stream and never drops state. A real switch:
 *   - clears session-scoped host state exactly like the existing switch path
 *     (the FaktorStore drops the session-owned snapshot panels on the id
 *     change; uploads and the pending submission identity are cleared here),
 *   - persists the explicit choice as the durable workspace binding so a
 *     later `ensureSession` (fresh daemon) resolves to it instead of bouncing
 *     back,
 *   - loads the durable messages from the start with the existing bounded
 *     page, refreshes the snapshot and restarts the stream at cursor 0.
 * An unsent composer draft is NEVER redirected: it stays in the composer
 * (the same draft policy as a refused start) while the chosen conversation
 * loads its own durable transcript.
 */
async function switchSession(
  sessionId: string,
  context: vscode.ExtensionContext,
): Promise<void> {
  const decision = sessionSwitchDecision(active.sessionId, sessionId);
  if (decision.action === 'noop') {
    // A blank/already-active request changes nothing, but the panel is
    // answered with the current state so its in-flight Open lock releases.
    chatProvider?.postSnapshot(store.snapshot());
    return;
  }
  const client = active.client;
  if (!client) {
    chatProvider?.postNotice('info', 'start the Faktor service before switching conversations');
    return;
  }
  active.sessionId = decision.sessionId;
  bumpLifecycle();
  conversationViewEpoch += 1;
  // Item 13: the accumulated transcript, its scrollback cursor and the
  // per-conversation model override belong to ONE conversation.
  active.transcriptSessionId = null;
  active.transcriptHasMore = false;
  active.transcriptNextBefore = null;
  active.futureModelOverride = null;
  streamRefresh?.reset();
  if (active.refreshTimer !== null) {
    clearTimeout(active.refreshTimer);
    active.refreshTimer = null;
  }
  active.activeRunId = null;
  // Turn lifecycle is per conversation: a switch starts a fresh one (the
  // new session's subscription drives it from its own handshake).
  resetTurnLifecycle();
  active.taskVerificationCache.clear();
  active.taskProofKey = null;
  active.taskProof = null;
  active.taskProofUnavailable = null;
  active.tournamentId = null;
  active.pixelPresence = new Map();
  active.completionContract = null;
  active.askOnly = false;
  active.boardSeenRevision = 0;
  active.billingCursor = null;
  active.billingPrevCursors = [];
  taskStartGate.reset();
  if (composerAttachmentsSessionId !== null && composerAttachmentsSessionId !== decision.sessionId) {
    // Uploads and the pending submission identity are bound to ONE session;
    // the visible set cannot leak into the chosen conversation.
    composerAttachments.clear();
    chatProvider?.postAttachmentsCleared();
  }
  composerAttachmentsSessionId = decision.sessionId;
  chatProvider?.postStreamBlocked(null);
  // The explicit choice is the durable binding for this exact workspace:
  // ensureSession() (`boundSessionFor`) resolves it on a fresh daemon instead
  // of rebinding the previous conversation.
  const workspace = canonicalWorkspace(context);
  const bindings = withBinding(readBindings(context), workspace.key, decision.sessionId);
  await writeBindings(context, bindings);
  const listed = store.snapshot().sessions.find((entry) => entry.id === decision.sessionId);
  store.patch({
    session:
      listed ?? {
        id: decision.sessionId,
        title: '',
        provider: '',
        model: '',
        state: 'unknown',
        createdMs: null,
      },
    activeRunId: null,
    busy: false,
    transcript: [],
    transcriptRevision: '',
    streamStatus: 'stopped',
    lastError: null,
    lastErrorKind: null,
  });
  try {
    const page = await client.messages(decision.sessionId, { limit: HISTORY_PAGE_LIMIT });
    const transcript = transcriptOf(page);
    store.patch({ transcript, transcriptRevision: transcriptSignature(transcript) });
  } catch (error) {
    recordBackgroundError(error);
  }
  await refresh();
  startStream();
  const label =
    listed !== undefined && listed.title.length > 0
      ? `"${listed.title}"`
      : 'the chosen conversation';
  chatProvider?.postNotice('info', `switched to ${label}`);
}

/**
 * The History surface's Refresh action: re-read ONLY the durable session
 * listing. A refusal records the explicit unavailable state (T31); a stopped
 * service is an explicit notice, never an implicit start.
 */
async function refreshHistory(): Promise<void> {
  const client = active.client;
  if (!client) {
    chatProvider?.postNotice('info', 'start the Faktor service to read the conversation list');
    return;
  }
  try {
    const sessions = await client.listSessions();
    store.patch({ sessions, sessionsUnavailable: null });
  } catch (error) {
    store.patch({ sessionsUnavailable: messageOf(error) });
  }
}

/**
 * `Faktor: Switch Conversation` — a native QuickPick over the SAME served
 * durable listing the History surface renders: title + served state (no
 * fabricated date, no guessed session).
 */
/**
 * Bind the picked provider/model to a NEW conversation when the current one
 * has a different binding. The selection is also saved as the default; the
 * current transcript is not rewritten and an unsent draft is preserved by
 * `switchSession` (the composer is never redirected/cleared by a model
 * change). Returns the user-facing outcome copy.
 */
async function newConversationWith(
  provider: string,
  model: string,
  context: vscode.ExtensionContext,
): Promise<string> {
  const client = active.client;
  if (client === null) {
    return 'Start the Faktor service, then choose again.';
  }
  const current = store.snapshot().session;
  if (current !== null && current.provider === provider && current.model === model) {
    return 'The current conversation already uses this model.';
  }
  try {
    const previous = active.sessionId;
    const created = await createBoundSession(client, context, provider, model);
    active.sessionId = previous;
    await switchSession(created.id, context);
    return 'A new conversation now uses it; your draft is preserved.';
  } catch (error) {
    return `The default was saved, but the new conversation could not start: ${messageOf(error)}`;
  }
}

async function switchSessionFromCommand(context: vscode.ExtensionContext): Promise<void> {
  const client = active.client;
  if (!client) {
    chatProvider?.postNotice('info', 'start the Faktor service before switching conversations');
    return;
  }
  let sessions: NativeSessionSummary[];
  try {
    sessions = await client.listSessions();
    store.patch({ sessions, sessionsUnavailable: null });
  } catch (error) {
    store.patch({ sessionsUnavailable: messageOf(error) });
    reportError(error);
    return;
  }
  if (sessions.length === 0) {
    chatProvider?.postNotice('info', 'no durable conversations are served');
    return;
  }
  const currentSessionId = active.sessionId;
  const picked = await vscode.window.showQuickPick(
    sessions.map((entry) => ({
      label: entry.title.length > 0 ? entry.title : 'Untitled conversation',
      description: `${entry.id === currentSessionId ? 'current · ' : ''}${entry.state}`,
      detail: `${entry.provider} / ${entry.model}`,
      sessionId: entry.id,
    })),
    {
      title: 'Faktor: Switch Conversation',
      placeHolder: 'Select the durable conversation to open',
      ignoreFocusOut: true,
    },
  );
  if (picked === undefined) {
    return;
  }
  await switchSession(picked.sessionId, context);
}

function transcriptOf(page: NativeMessagePage): TranscriptEntry[] {
  return transcriptFromMessages(page.messages as unknown as Json[]);
}

// ------------------------------------------------- turn lifecycle + live output
//
// The ONE turn reducer (idle → submitting → queued/starting → streaming/
// tool-running/awaiting-user → completed | interrupted | failed). It owns
// `busy` while a turn is live (the Ask path previously cleared busy on HTTP
// ACCEPTANCE, showing Ready with no Stop mid-reply) and names the exact
// operation Stop aborts.
let turns: TurnLifecycle = emptyTurnLifecycle();
/** The last (display phase, stop op) posted to the panel (dedupe). */
let lastPostedTurnKey: string | null = null;

function dispatchTurn(event: TurnEvent): void {
  const wasLive = turnLifecycleIsLive(turns);
  turns = turnReducer(turns, event);
  const isLive = turnLifecycleIsLive(turns);
  if (wasLive !== isLive) {
    store.patch({ busy: isLive || active.activeRunId !== null });
  }
  const phase = turnDisplayPhase(turns);
  const stopOp = turnStopOpId(turns);
  const key = `${phase}:${stopOp ?? ''}`;
  if (key !== lastPostedTurnKey) {
    lastPostedTurnKey = key;
    chatProvider?.postTurnState(phase, stopOp);
  }
}

function resetTurnLifecycle(): void {
  turns = emptyTurnLifecycle();
  lastPostedTurnKey = null;
  chatProvider?.postTurnState('idle', null);
}

/** Host-side live batching: frames accumulate for one flush window. */
const LIVE_FLUSH_MS = 16;
/** Bound of frames held in ONE flush (beyond it: flush now). */
const LIVE_FLUSH_FRAME_CAP = 128;
let livePendingFrames: LiveChunkFrame[] = [];
let liveFlushTimer: NodeJS.Timeout | null = null;
/** The instance id of the last handshake (frames from any other instance
 * are a foreign lineage after a daemon restart and are dropped). */
let liveExpectedInstance: string | null = null;
/** Single-flight + cooldown of the authoritative resync fetch. */
let liveResyncInFlight = false;
let liveResyncLastAt = 0;

function queueLiveFrame(frame: LiveChunkFrame): void {
  if (active.sessionId === null || String(frame.session) !== active.sessionId) {
    // Frames of a previous session can never reach the current panel.
    return;
  }
  if (liveExpectedInstance !== null && frame.instance !== liveExpectedInstance) {
    return;
  }
  livePendingFrames.push(frame);
  if (livePendingFrames.length >= LIVE_FLUSH_FRAME_CAP) {
    flushLiveFrames();
    return;
  }
  if (liveFlushTimer === null) {
    liveFlushTimer = setTimeout(() => {
      liveFlushTimer = null;
      flushLiveFrames();
    }, LIVE_FLUSH_MS);
  }
}

/** Deliver the accumulated batch as ONE panel message (one DOM frame). */
function flushLiveFrames(): void {
  if (liveFlushTimer !== null) {
    clearTimeout(liveFlushTimer);
    liveFlushTimer = null;
  }
  if (livePendingFrames.length === 0) {
    return;
  }
  const frames = livePendingFrames;
  livePendingFrames = [];
  chatProvider?.postLiveOutput(frames);
}

function clearLiveBatch(): void {
  if (liveFlushTimer !== null) {
    clearTimeout(liveFlushTimer);
    liveFlushTimer = null;
  }
  livePendingFrames = [];
}

/**
 * The explicit resync: fetch the AUTHORITATIVE part snapshot (by part id/
 * revision/offset/watermark) and hand it to the panel, which reconciles so
 * a saved page never duplicates live text nor overwrites a newer suffix.
 * Single-flight with a bounded cooldown (a resync storm cannot happen).
 */
async function requestLiveResync(reason: string): Promise<void> {
  const daemon = active.daemon;
  const sessionId = active.sessionId;
  if (daemon === null || sessionId === null) {
    return;
  }
  const now = Date.now();
  if (liveResyncInFlight || now - liveResyncLastAt < 250) {
    return;
  }
  liveResyncInFlight = true;
  liveResyncLastAt = now;
  try {
    const snapshot = await fetchLiveSnapshot(daemon.baseUrl, daemon.bearerToken, sessionId, {
      fetch: sseAdapter(),
    });
    applyLiveSnapshot(snapshot);
  } catch (error) {
    recordBackgroundError(error instanceof Error ? error : new Error(`${reason}: ${String(error)}`));
  } finally {
    liveResyncInFlight = false;
  }
}

function applyLiveSnapshot(snapshot: {
  readonly instance: string;
  readonly session: number;
  readonly turn: { readonly op: string; readonly status: string } | null;
}): void {
  if (active.sessionId === null || String(snapshot.session) !== active.sessionId) {
    return;
  }
  // A new instance (daemon restart) makes every buffered frame foreign.
  if (liveExpectedInstance !== snapshot.instance) {
    if (liveExpectedInstance !== null) {
      clearLiveBatch();
    }
    liveExpectedInstance = snapshot.instance;
  }
  if (snapshot.turn !== null) {
    dispatchTurn({ type: 'turnStatus', opId: snapshot.turn.op, status: snapshot.turn.status });
  }
  chatProvider?.postLiveSnapshot(snapshot);
}

function handleLiveMessage(message: LiveOutputMessage): void {
  // NOTE: if/else (not `case`) — the incoming webview-message switch owns
  // `case` labels; this live-transport dispatch is host-internal.
  if (message.type === 'snapshot') {
    // The subscribe-before-submit handshake (and every reconnect
    // baseline): exact starting watermark + authoritative parts.
    applyLiveSnapshot(message.snapshot);
    return;
  }
  if (message.type === 'turn') {
    // The durable turn projection: the ONLY completion authority.
    const { turn, status } = message.frame;
    if (turn !== null && status !== null) {
      dispatchTurn({ type: 'turnStatus', opId: turn, status });
    }
    return;
  }
  if (message.type === 'resync') {
    // The bounded per-subscriber queue overran: explicit signal, then
    // the authoritative reconciliation (never silent loss).
    void requestLiveResync(`lagged: ${message.frame.reason}`);
    return;
  }
  if (message.type === 'live') {
    queueLiveFrame(message.frame);
  }
}

/**
 * Subscribe to the session's live model output (idempotent). Call BEFORE a
 * prompt POST so the handshake snapshot lands first and a fast reply can
 * never outrun the listener (subscribe-before-submit).
 */
function startLiveOutput(): void {
  const daemon = active.daemon;
  const sessionId = active.sessionId;
  if (!daemon || !sessionId) {
    return;
  }
  active.liveStream?.stop();
  clearLiveBatch();
  const stream = new LiveOutputStream({
    baseUrl: daemon.baseUrl,
    bearerToken: daemon.bearerToken,
    sessionId,
    fetch: sseAdapter(),
    onMessage: handleLiveMessage,
    onStatus: (status: LiveOutputStreamStatus) => {
      if (status === 'stopped' || status === 'retrying') {
        // A reconnect will re-handshake with a fresh snapshot: everything
        // buffered under the old connection is stale by definition.
        clearLiveBatch();
      }
    },
    onError: (error) => {
      recordBackgroundError(error);
    },
  });
  active.liveStream = stream;
  stream.start();
}

/** Stop the live subscription (daemon crash/stop, session switch). */
function stopLiveOutput(): void {
  active.liveStream?.stop();
  active.liveStream = null;
  clearLiveBatch();
}

function startStream(cursor = 0): void {
  const daemon = active.daemon;
  const sessionId = active.sessionId;
  if (!daemon || !sessionId) {
    return;
  }
  active.stream?.stop();
  const stream = new EventStream({
    baseUrl: daemon.baseUrl,
    bearerToken: daemon.bearerToken,
    sessionId,
    cursor: Number.isInteger(cursor) && cursor > 0 ? cursor : undefined,
    fetch: sseAdapter(),
    onEvent: (frame) => {
      // The journal stream is a LIVENESS signal only: its `EventKind` frames
      // do not carry the bounded message/part page shapes, so the reducer
      // deliberately does not fabricate incremental transcript state from
      // them. Frames coalesce into one bounded page refresh per window, so a
      // continuous 200/s stream can neither starve the refresh (a trailing
      // debounce) nor refresh 200 times a second.
      //
      // A `failed` frame DOES carry the typed reason the durable session
      // entered `failed_recoverable` (provider failure, ambiguous
      // instruction root, ...). The projection has no error field, so without
      // this the panel showed only "failed, retrying" and no cause. Surface
      // that one bounded reason once through the user-error renderer; the
      // page refresh below remains the source of all structured state.
      if (frame.event === 'failed') {
        const data = frame.data;
        const payload =
          data !== null && typeof data === 'object' && !Array.isArray(data)
            ? (data as Record<string, unknown>).payload
            : null;
        const message =
          payload !== null && typeof payload === 'object' && !Array.isArray(payload)
            ? (payload as Record<string, unknown>).message
            : null;
        if (typeof message === 'string' && message.length > 0) {
          const projection = projectOf(new Error(message), 'run_failure');
          store.patch({ lastError: projection.technical, lastErrorKind: projection.kind });
          chatProvider?.postUserError(projection);
        }
      }
      // The durable journal drives the ONE turn reducer: admission,
      // machine-state tags, tool phases and the terminal events. Terminal
      // transitions only ever come from these durable events or the turn
      // record — never from a quiet stream or an HTTP 2xx.
      {
        const data = frame.data;
        const record =
          data !== null && typeof data === 'object' && !Array.isArray(data)
            ? (data as Record<string, unknown>)
            : null;
        dispatchTurn({
          type: 'journal',
          kind: frame.event,
          opId: typeof record?.opId === 'string' ? record.opId : null,
          state: typeof record?.state === 'string' ? record.state : undefined,
        });
      }
      streamRefresh?.request();
    },
    onStatus: (status: EventStreamStatus, detail) => {
      if (status === 'protocol_blocked') {
        // A durable event the client cannot consume is terminal: keep the
        // exact reason, patch the typed unavailable status and surface the
        // recovery affordances (refresh / reconnect / service doctor). No
        // automatic reconnect loop replays the offending frame.
        const reason = detail ?? 'the durable event stream is blocked';
        const projection = projectOf(new Error(reason), 'stream_gap');
        store.patch({ streamStatus: status, lastError: projection.technical, lastErrorKind: projection.kind });
        chatProvider?.postUserError(projection);
        chatProvider?.postStreamRecovery(false);
        chatProvider?.postStreamBlocked(reason, active.stream?.blocked?.cursor ?? null);
      } else {
        store.patch({ streamStatus: status });
        // The stream is a transport signal, not process ownership: a
        // retrying SSE connection does not mean the daemon died. The
        // supervisor owns restart. Only the transient notice changes here.
        if (status === 'retrying') {
          store.patch({ connectionNotice: 'reconnecting' });
        } else if (status === 'open') {
          store.patch({ connectionNotice: null });
        }
        if (status === 'open' || status === 'stopped') {
          // A recovered/stopped stream leaves the T27 reconnecting state.
          chatProvider?.postStreamRecovery(false);
        }
        if (status === 'open' || status === 'connecting') {
          chatProvider?.postStreamBlocked(null);
        }
      }
      updateStatusBar();
    },
    onError: (error) => {
      recordBackgroundError(error);
    },
  });
  active.stream = stream;
  stream.start();
  store.patch({ streamStatus: 'connecting' });
  chatProvider?.postStreamRecovery(false);
  // The live model-output subscription rides the same lifecycle (and the
  // same session): its handshake snapshot is the baseline every later
  // snapshot/frame reconciles against.
  startLiveOutput();
}

// -------------------------------------------------------------------- refresh

function scheduleRefresh(delayMs = 250): void {
  if (active.refreshTimer !== null) {
    clearTimeout(active.refreshTimer);
  }
  active.refreshTimer = setTimeout(() => {
    active.refreshTimer = null;
    void refresh();
  }, delayMs);
}

/**
 * The user-gesture refresh path. Scheduled and internal refreshes stay
 * silent, but a palette command or the panel button must never be a silent
 * no-op: an unavailable daemon/session or an in-flight refresh answers with
 * a visible typed notice.
 */
async function refreshFromUserGesture(): Promise<void> {
  if (!active.client || !active.sessionId) {
    recordError(projectOf(new Error('no active service connection'), 'daemon_down'));
    return;
  }
  if (active.refreshing) {
    chatProvider?.postNotice('info', 'a refresh is already in flight');
    return;
  }
  await refresh();
}

async function refresh(): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  if (!client || !sessionId || active.refreshing) {
    return;
  }
  // Fence every await against lifecycle transitions: this refresh may only
  // publish if the exact daemon + session generation it captured is still
  // current when it finishes.
  const generation = lifecycle.current();
  const current = (): boolean =>
    lifecycle.isCurrent(generation) &&
    active.client === client &&
    active.sessionId === sessionId;
  const plan = refreshReadPlan({ askOnly: active.askOnly });
  if (plan.includes('conversation')) {
    await refreshConversation();
    return;
  }
  active.refreshing = true;
  try {
    // The board read is OPTIONAL and never rejects: an older daemon records
    // an explicit unavailable block instead of blanking the snapshot.
    const boardPromise = boardFor(client, sessionId);
    // The projection read is FAILURE-ISOLATED (I03): its `filesChanged` feeds
    // the Changes view, and a refusal becomes that view's explicit
    // unavailable state with the reason — never a whole-refresh failure that
    // would blank unrelated durable reads (or a stale change list).
    const projectionRead = client.projection(sessionId).then(
      (value) => ({ value, reason: null as string | null }),
      (error) => ({ value: null as NativeProjection | null, reason: messageOf(error) }),
    );
    const [
      projection,
      tasks,
      verification,
      usage,
      agents,
      runs,
      sessionListing,
      messages,
      catalog,
      permissionList,
    ] = await Promise.all([
      projectionRead,
      client.tasks(sessionId),
      client.verification(sessionId),
      client.sessionUsage(sessionId),
      client.agents(sessionId),
      client.taskRuns(sessionId),
      // The listing is FAILURE-ISOLATED: its refusal becomes the History
      // surface's explicit unavailable state (never a whole-refresh failure,
      // which would also blank unrelated durable reads).
      client.listSessions().then(
        (list) => ({ list, reason: null as string | null }),
        (error) => ({ list: null as NativeSessionSummary[] | null, reason: messageOf(error) }),
      ),
      client.messages(sessionId, { limit: HISTORY_PAGE_LIMIT }),
      // The catalog only enriches agent metadata; a catalog failure must
      // not blank the rest of the snapshot.
      client.modelCatalog().catch(() => [] as NativeModelInfo[]),
      // The pending-approval read is OPTIONAL: an older daemon or a
      // transient refusal keeps the last known set instead of hiding a
      // waiting approval behind a blank snapshot.
      client.permissions(sessionId).then(
        (list) => list,
        () => null,
      ),
    ]);
    const board = await boardPromise;
    if (!current()) {
      return;
    }
    const projectionValue = projection.value;
    // Durable index coverage (audits 5/6): optional like the board read — an
    // older daemon or a never-hosted index service yields null, never a
    // snapshot failure.
    const indexCoverage = await indexCoverageFor(client, sessionId);
    const task =
      tasks.length > 0
        ? taskSummary(tasks[0]!, runs[0]?.state ?? null, active.completionContract)
        : null;
    // busy/activeRunId are DERIVED FROM THE RUN STATE: a terminal run
    // (Done/Failed/Cancelled) is not running merely because it is listed.
    const activeRunId = activeRunIdAfter(active.activeRunId, runs.map(runSummary));
    active.activeRunId = activeRunId;
    const agentFrame = agents as unknown as Json[];
    const agentSummaries = summarizeAgents(
      agentFrame,
      catalog.map(modelInfoOf),
      active.pixelPresence,
    );
    active.pixelPresence = nextPixelPresence(active.pixelPresence, agentFrame);
    const verificationView = verificationSummary(verification);
    const usageView = usageSummary(usage);
    const taskRevision = tasks.length > 0 ? tasks[0]!.revision : null;
    const taskVerification = await taskVerificationFor(
      client,
      sessionId,
      runs,
      task,
      verification,
      taskRevision,
    );
    // The strict proof summary feeds the top-level VERIFIED view: a failed
    // read is an EXPLICIT unavailable state (the cached proof is dropped),
    // so a stale VERIFIED can never render for an unreadable proof.
    const proofRead = await taskProofFor(client, sessionId, runs, task);
    // The durable tournament auto-load: the tracked id while it still exists,
    // else the newest summary. A missing listing/state is a null block, never
    // a fabricated tournament.
    const tournament = tournamentViewOf(await tournamentFor(client, sessionId));
    // Commercial data is deliberately NOT read here. The run cockpit and
    // normal refresh never touch billing, account, subscription or
    // entitlement routes; the explicit Account/Usage surface is the only
    // reader.
    // The cockpit projection is FAILURE-ISOLATED: a malformed verification
    // or cockpit payload degrades the cockpit panel to an explicit error
    // while every other snapshot field still patches.
    const cockpitProjection = projectCockpit({
      task,
      agents: agentSummaries,
      verification: verificationView,
      usage: usageView,
      taskVerification: cockpitTaskVerificationView(taskVerification.view),
      taskVerificationUnavailable: taskVerification.unavailable,
      tournament,
      proof: proofRead.proof,
      proofUnavailable: proofRead.unavailable,
    });
    // The authority may have changed while the reads ran: never patch a
    // newer snapshot with this stale generation's data.
    if (!current()) {
      return;
    }
    store.patch({
      ...(sessionListing.reason === null && sessionListing.list !== null
        ? { sessions: sessionListing.list, sessionsUnavailable: null }
        : {
            sessionsUnavailable:
              sessionListing.reason ?? 'the conversation list could not be read',
          }),
      // A failed projection read keeps the last known machine state (the
      // Changes view carries the explicit reason) instead of blanking it.
      machineState: projectionValue?.state.machine ?? store.snapshot().machineState,
      machineLabel: projectionValue?.state.label ?? store.snapshot().machineLabel,
      task,
      // T12's only served source: the proof body's own embedded task state
      // (`verifying` / `needs_verification`). Absent/unreadable proof => null;
      // it is never inferred from the collapsed session tag.
      proofTaskState: proofRead.proof?.task?.state ?? null,
      verification: verificationView,
      usage: usageView,
      agents: agentSummaries,
      permissions:
        permissionList === null ? store.snapshot().permissions : permissionSummaries(permissionList),
      runs: runs.map(runSummary),
      activeRunId,
      // The turn reducer owns busy for Ask turns (a Task run still maps to
      // activeRunId): a live turn NEVER displays as Ready without Stop.
      busy: activeRunId !== null || turnLifecycleIsLive(turns),
      cockpit: cockpitProjection.cockpit,
      cockpitSections: cockpitProjection.sections,
      tournament: cockpitProjection.cockpit?.tournament ?? null,
      board,
      // I03: the served candidate-change view. Only served relationships
      // attach refs; a projection failure is its explicit unavailable state.
      changes: buildChangesSummary({
        filesChanged: projectionValue?.filesChanged ?? [],
        verification: taskVerification.view,
        proof: proofRead.proof,
        unavailable: projection.reason,
        candidateDisplay: candidateDisplayOf(
          proofRead.proof?.task?.goal ?? task?.goal ?? null,
        ),
      }),
      indexCoverage,
      lastError: cockpitProjection.error,
    });
    // Assistant/status/tool lines are durable message rows; re-render the
    // bounded newest page only when its structure actually changed.
    const transcript = transcriptOf(messages);
    const transcriptRevision = transcriptSignature(transcript);
    const previous = store.snapshot();
    if (
      transcriptRevision !== previous.transcriptRevision ||
      // A digest is a cheap gate, never an authority: a collision must not
      // suppress a genuinely different (same-length/same-digest) document.
      !transcriptContentEqual(transcript, previous.transcript)
    ) {
      store.patch({ transcript, transcriptRevision });
    }
  } catch (error) {
    if (current()) {
      recordBackgroundError(error);
    }
  } finally {
    // Only the generation that armed the flag may clear it; a transition
    // already disarmed it (bumpLifecycle) and may have armed a newer refresh.
    if (lifecycle.isCurrent(generation)) {
      active.refreshing = false;
    }
    updateStatusBar();
  }
}

/**
 * The Ask/conversational refresh: transcript, session listing and pending
 * approvals only. Deliberately no task, task-run, verification, proof,
 * tournament or billing read. The shared `loadConversation` helper is the
 * executable boundary and the live harness drives the same helper.
 */
async function refreshConversation(): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  if (!client || !sessionId || active.refreshing) {
    return;
  }
  const generation = lifecycle.current();
  const current = (): boolean =>
    lifecycle.isCurrent(generation) &&
    active.client === client &&
    active.sessionId === sessionId;
  active.refreshing = true;
  try {
    const read = await loadConversation(client, sessionId, HISTORY_PAGE_LIMIT);
    if (!current()) {
      return;
    }
    const pageEntries = transcriptOf(read.messages);
    // Item 13: pages MERGE by durable message id — the newest page never
    // replaces the accumulated transcript, so a scrollback that loaded
    // older history survives every later refresh. A different session
    // (first load after a switch) starts a fresh accumulation.
    const previous = store.snapshot();
    const accumulated =
      active.transcriptSessionId === sessionId
        ? mergeTranscriptPages(previous.transcript, pageEntries)
        : pageEntries;
    active.transcriptSessionId = sessionId;
    active.transcriptHasMore = read.messages.hasMore === true;
    active.transcriptNextBefore = read.messages.nextBefore;
    const transcriptRevision = transcriptSignature(accumulated);
    store.patch({
      sessions: read.sessions,
      sessionsUnavailable: null,
      transcriptHasMore: active.transcriptHasMore,
      permissions:
        read.permissions === null
          ? previous.permissions
          : permissionSummaries(read.permissions as NativePermissionEntry[]),
      ...(transcriptRevision !== previous.transcriptRevision ||
      !transcriptContentEqual(accumulated, previous.transcript)
        ? { transcript: accumulated, transcriptRevision }
        : {}),
    });
  } catch (error) {
    if (current()) {
      recordBackgroundError(error);
    }
  } finally {
    if (lifecycle.isCurrent(generation)) {
      active.refreshing = false;
    }
    updateStatusBar();
  }
}

/** Catalog -> agent summary metadata (provider/reasoning/thinking). */
function modelInfoOf(info: NativeModelInfo): {
  provider: string;
  model: string;
  reasoning: boolean;
  thinking: boolean;
  tools: boolean;
} {
  return {
    provider: info.provider,
    model: info.model,
    reasoning: info.reasoning,
    thinking: info.thinking,
    tools: info.tools,
  };
}

/**
 * Fetch the durable task verification (criteria + checks) for the cockpit,
 * reusing the cached read while its AUTHORITY KEY is unchanged. The key is
 * durable identity (task id + state + the task view's revision) plus a
 * fingerprint of the FULL verification summary response — never list counts.
 * A key change or a failed fetch drops the cached view and returns an
 * explicit unavailable; the previous view can never masquerade as current.
 */
async function taskVerificationFor(
  client: NativeClient,
  sessionId: string,
  runs: readonly NativeTaskRun[],
  task: TaskSummary | null,
  verification: NativeVerificationView,
  revision: string | null,
): Promise<CachedEvidenceView<NativeTaskVerification>> {
  if (task === null || runs.length === 0) {
    active.taskVerificationCache.clear();
    return { view: null, unavailable: null };
  }
  const taskId = String(runs[0]!.task_id);
  const key = verificationAuthorityKey({
    taskId,
    state: task.state,
    revision,
    verification,
  });
  const cached = active.taskVerificationCache.read(key);
  if (cached !== null) {
    return cached;
  }
  try {
    const view = await client.taskVerification(sessionId, taskId);
    active.taskVerificationCache.store(key, view);
    return { view, unavailable: null };
  } catch (error) {
    const reason = messageOf(error);
    active.taskVerificationCache.fail(key, reason);
    return { view: null, unavailable: reason };
  }
}

/**
 * Fetch the strict proof summary (`GET /native/tasks/{id}/proof`) for the
 * top-level VERIFIED view, reusing the cached read while its inputs are
 * unchanged. A refusal (typed 4xx/5xx, protocol violation) drops the cached
 * proof and returns the error text as an EXPLICIT unavailable reason; a
 * stale VERIFIED is never rendered for a proof that can no longer be read.
 */
async function taskProofFor(
  client: NativeClient,
  sessionId: string,
  runs: readonly NativeTaskRun[],
  task: TaskSummary | null,
): Promise<{ proof: NativeTaskProof | null; unavailable: string | null }> {
  if (task === null || runs.length === 0) {
    active.taskProofKey = null;
    active.taskProof = null;
    active.taskProofUnavailable = null;
    return { proof: null, unavailable: null };
  }
  const taskId = String(runs[0]!.task_id);
  const key = `${taskId}:${task.state}:${runs[0]!.state}`;
  if (key === active.taskProofKey) {
    return { proof: active.taskProof, unavailable: active.taskProofUnavailable };
  }
  try {
    const proof = await client.taskProof(sessionId, taskId);
    active.taskProofKey = key;
    active.taskProof = proof;
    active.taskProofUnavailable = null;
    return { proof, unavailable: null };
  } catch (error) {
    const reason = messageOf(error);
    active.taskProofKey = key;
    active.taskProof = null;
    active.taskProofUnavailable = reason;
    return { proof: null, unavailable: reason };
  }
}

/**
 * Read the commercial-metering surface: the control-plane identity fixes the
 * organization, then one entitlement snapshot and one cursor page of the
 * usage fold. Every refusal is captured as the panel's explicit
 * disabled/unavailable reason — the panel NEVER shows stale numbers behind a
 * refusal, and a `billing_disabled` refusal renders "billing disabled
 * locally" instead of zeros.
 */
async function billingPanelFor(client: NativeClient): Promise<CockpitUsagePanel> {
  const cursor = active.billingCursor;
  const hasPrev = active.billingPrevCursors.length > 0;
  try {
    const identityView = await client.identity();
    active.billingIdentity = identityView.identity;
    const [entitlementsView, usage, overview] = await Promise.all([
      client.entitlements(),
      client.billingUsage(identityView.identity.organization, {
        since: cursor,
        limit: BILLING_PAGE_LIMIT,
      }),
      client.billingOverview(),
    ]);
    active.billingEntitlements = entitlementsView.entitlements;
    active.billingUsage = usage;
    active.billingOverview = overview.billing;
    return buildUsagePanel({
      identity: identityView.identity,
      entitlements: entitlementsView.entitlements,
      usage,
      billing: overview.billing,
      refusal: null,
      cursor,
      hasPrev,
    });
  } catch (error) {
    const refusal =
      error instanceof NativeApiError
        ? { code: error.code, reason: `${error.status} ${error.code}: ${error.message}` }
        : { code: 'unavailable', reason: messageOf(error) };
    active.billingUsage = null;
    active.billingEntitlements = null;
    active.billingOverview = null;
    return buildUsagePanel({
      identity: active.billingIdentity,
      entitlements: null,
      usage: null,
      billing: null,
      refusal,
      cursor,
      hasPrev,
    });
  }
}

/**
 * The explicit Account/Usage surface. This is the ONLY billing/entitlement
 * reader: it is never called by `refresh()` or by any chat/run path. With no
 * control-plane credential it reports `Account not connected` and performs
 * no network work at all.
 */
async function readAccountSurface(): Promise<void> {
  const client = active.client;
  if (!client) {
    store.patch({ usagePanel: null, accountStatus: 'not_connected' });
    return;
  }
  if (!client.hasControlToken()) {
    store.patch({ usagePanel: null, accountStatus: 'not_connected' });
    return;
  }
  const panel = await billingPanelFor(client);
  store.patch({
    usagePanel: panel,
    accountStatus: panel.state === 'ok' ? 'read' : 'unavailable',
  });
}

/**
 * The cause-specific recovery actions from the shared UX model. Only the
 * host-owned actions land here; local view actions (focus composer, switch
 * to checks, open diagnostics) stay in the webview.
 */
async function handleUxAction(raw: unknown): Promise<void> {
  const action = typeof raw === 'string' ? raw : '';
  if (action === 'openProviderSettings') {
    await vscode.commands.executeCommand('faktor.openProviders8');
    return;
  }
  if (action === 'reconnect') {
    // Item 11: Reconnect ACTUALLY reconnects — a blocked stream recovers
    // from its paused cursor; otherwise the service connection itself is
    // re-established (never a silent refresh masquerading as a reconnect).
    if (active.stream?.blocked != null) {
      recoverEventStream();
      return;
    }
    if (serviceLifecycle !== null) {
      await serviceLifecycle.reconnectNow();
    }
    await refreshFromUserGesture();
    return;
  }
  if (action === 'resumeRun') {
    // Item 11: Resume is a REAL typed dispatch. A paused CHILD agent
    // resumes through the daemon's agent-control route; a blocked stream
    // recovers first so the resumed turn is observable. The serving
    // daemon exposes NO session-level run-resume route (only cancel), so
    // a paused conversation-level run gets an honest refusal naming the
    // missing route contract — never a fake "resumed" notice.
    if (active.stream?.blocked != null) {
      recoverEventStream();
    }
    const client = active.client;
    const sessionId = active.sessionId;
    if (!client || !sessionId) {
      chatProvider?.postNotice('error', 'the Faktor service is not connected; nothing was resumed');
      return;
    }
    const pausedChild = store
      .snapshot()
      .agents.find((agent) => agent.kind === 'child' && agent.state === 'paused');
    if (pausedChild !== undefined) {
      try {
        await client.resumeAgent(pausedChild.agentId);
        chatProvider?.postNotice('info', `resumed the paused agent ${pausedChild.agentId}`);
        scheduleRefresh(0);
      } catch (error) {
        reportError(error);
      }
      return;
    }
    chatProvider?.postNotice(
      'error',
      'this serving daemon has no resume route for a paused conversation run ' +
        '(POST /native/session/{id}/task-runs/{run_id}/resume is not served); ' +
        'the run remains saved — nothing was re-sent',
    );
    await refreshFromUserGesture();
    return;
  }
}

/**
 * Auto-load the durable tournament the cockpit tracks: the tracked id while
 * the listing still names it, else the newest summary. Any listing/read
 * failure degrades to null (no tournament block), never an error patch.
 */
async function tournamentFor(
  client: NativeClient,
  sessionId: string,
): Promise<NativeTournament | null> {
  let summaries: NativeTournamentSummary[];
  try {
    summaries = await client.tournaments(sessionId);
  } catch {
    active.tournamentId = null;
    return null;
  }
  const tracked = active.tournamentId;
  const target =
    tracked !== null && summaries.some((summary) => summary.id === tracked)
      ? tracked
      : summaries.length > 0
        ? summaries[summaries.length - 1]!.id
        : null;
  if (target === null) {
    active.tournamentId = null;
    return null;
  }
  try {
    const state = await client.tournamentState(sessionId, target);
    active.tournamentId = state.id;
    return state;
  } catch {
    return null;
  }
}

/** TRUE when the serving daemon predates the additive native board route. */
function boardRouteMissing(error: unknown): boolean {
  return (
    error instanceof NativeApiError &&
    (error.status === 404 || error.status === 405 || error.status === 501)
  );
}

/**
 * The board block of the snapshot. The additive GET is optional: a daemon
 * that predates it records `available:false` with the typed route reason,
 * and any other failure records its message the same way. Posts are NEVER
 * fabricated, and automatic refreshes never move the read watermark.
 */
async function boardFor(client: NativeClient, sessionId: string): Promise<BoardStateSummary> {
  try {
    const page = await client.board(sessionId, { limit: BOARD_PAGE_LIMIT });
    return boardStateFromPage(page, active.boardSeenRevision).board;
  } catch (error) {
    if (boardRouteMissing(error)) {
      const api = error as NativeApiError;
      return unavailableBoardState(
        `the serving daemon exposes no coordination-board route (HTTP ${api.status} ${api.code})`,
      );
    }
    return unavailableBoardState(`board read failed: ${messageOf(error)}`);
  }
}

/**
 * Durable index coverage read (audits 5/6/16): optional like the board read;
 * a missing route (older daemon), an unhosted index service or a read
 * failure yields null, and the status bar simply omits the index suffix —
 * never a fabricated "current".
 */
async function indexCoverageFor(
  client: NativeClient,
  sessionId: string,
): Promise<NativeIndexCoverageSnapshot | null> {
  try {
    return await client.indexCoverage(sessionId);
  } catch {
    return null;
  }
}

/**
 * One explicit board read (panel gesture or older-page pagination). Only a
 * fresh top-of-board read (no `since` cursor) acknowledges the page and
 * moves the watermark; an older-page read keeps new posts unread.
 */
async function readBoard(since: number | null, limit: number | null): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  if (!client || !sessionId) {
    return;
  }
  try {
    const page = await client.board(sessionId, {
      ...(since !== null ? { since } : {}),
      ...(limit !== null ? { limit } : {}),
    });
    if (since === null) {
      active.boardSeenRevision = page.revision;
    }
    store.patch({ board: boardStateFromPage(page, active.boardSeenRevision).board });
  } catch (error) {
    if (boardRouteMissing(error)) {
      const api = error as NativeApiError;
      store.patch({
        board: unavailableBoardState(
          `the serving daemon exposes no coordination-board route (HTTP ${api.status} ${api.code})`,
        ),
      });
      return;
    }
    reportError(error);
  }
}

/** One bounded board post as the active session; the server is the guard. */
async function postBoard(message: ChatMessage): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  const token =
    typeof message.token === 'string' && message.token.length <= 128 ? message.token : null;
  const parsed = parseBoardPostRequest({
    subject: message.subject,
    body: message.body,
    refs: message.refs,
  });
  if ('reason' in parsed) {
    chatProvider?.postBoardRefused(token, parsed.reason);
    chatProvider?.postNotice('error', parsed.reason);
    return;
  }
  if (!client || !sessionId) {
    chatProvider?.postBoardRefused(token, 'daemon-not-running');
    chatProvider?.postNotice('info', 'start the daemon before posting to the board');
    return;
  }
  try {
    const post = await client.boardPost(sessionId, parsed);
    // The operator authored this post: it is read by definition.
    active.boardSeenRevision = Math.max(active.boardSeenRevision, post.revision);
    chatProvider?.postBoardPosted(post.revision, token);
    chatProvider?.postNotice('info', `board post #${post.revision} recorded`);
    scheduleRefresh(0);
  } catch (error) {
    // EVERY outcome answers the panel: a refusal must release its in-flight
    // lock without waiting for an unrelated snapshot.
    chatProvider?.postBoardRefused(
      token,
      error instanceof Error ? error.message : 'board post failed',
    );
    reportError(error);
  }
}

function cockpitTaskVerificationView(view: NativeTaskVerification | null): CockpitTaskVerification | null {
  if (view === null) {
    return null;
  }
  return {
    records: view.records.map((record) => ({
      recordId: record.recordId,
      status: record.status,
      startedMs: record.startedMs,
      completedMs: record.completedMs,
      treeHash: record.treeHash,
      criteria: record.criteria.map((criterion) => ({
        criterionKey: criterion.criterionKey,
        passed: criterion.passed,
        evidence: criterion.evidence,
        requirement: criterion.requirement,
        origin: criterion.origin,
        verdict: criterion.verdict,
        binding: criterion.binding,
      })),
      candidateProof: record.candidateProof,
      verifiedSnapshot: record.verifiedSnapshot,
      basedOnSnapshot: record.basedOnSnapshot,
      sourceCount: record.sourceCount,
      landedSnapshot: record.landedSnapshot,
      checks: record.checks.map((check) => ({
        check: check.check,
        status: check.status,
        required: check.required,
      })),
    })),
  };
}

function taskSummary(
  view: NativeTaskView,
  runState: string | null,
  submitted: NativeCompletionContract | null,
): TaskSummary {
  return {
    goal: view.goal,
    state: view.state,
    completed: view.milestones.completed,
    open: view.milestones.open,
    testsRun: view.tests.run,
    testsFailed: view.tests.failed,
    changedFiles: view.changedFiles,
    budget: view.budget
      ? {
          maxTokens: view.budget.maxTokens,
          spentTokens: view.budget.spentTokens,
          maxCostMicro:
            view.budget.maxCostMicro === null
              ? null
              : microToString(view.budget.maxCostMicro),
          spentCostMicro: microToString(view.budget.spentCostMicro),
          openReservedMicro: microToString(view.budget.openReservedMicro),
        }
      : null,
    acceptanceCriteria: view.acceptanceCriteria,
    plan: view.plan.map((step) => ({
      id: step.id,
      summary: step.summary,
      state: step.state,
      dependsOn: step.dependsOn,
    })),
    blockers: view.blockers.map((blocker) => blocker.detail),
    evidenceRefs: view.evidenceRefs,
    phase: view.phase,
    progress: view.progress,
    completion: completionSummaryOf(view, runState, submitted),
    contract: view.contract,
    contractMalformed: view.contractMalformed,
    contractDroppedRows: view.contractDroppedRows,
  };
}

function verificationSummary(view: NativeVerificationView): VerificationSummary {
  return {
    owed: view.owed.map((entry) => ({
      opId: entry.opId,
      tool: entry.tool,
      status: entry.status,
      effectStatus: entry.effectStatus,
    })),
    failedChecks: view.failedChecks.map((entry) => ({ id: entry.id, detail: entry.detail })),
  };
}

function usageSummary(usage: NativeSessionUsage): UsageSummary {
  const spentCosts: MicroMoney[] = [];
  const openCosts: MicroMoney[] = [];
  const maxCosts: MicroMoney[] = [];
  let maxMicro: MicroMoney | null = null;
  let truncated = false;
  for (const task of usage.tasks) {
    // Exact bigint aggregation: a session total may exceed 2^53-1 micro-USD
    // and must never pass through a float.
    spentCosts.push(task.budget.spentCostMicro);
    openCosts.push(task.budget.openReservedMicro);
    if (task.budget.maxCostMicro !== null) {
      maxCosts.push(task.budget.maxCostMicro);
    }
    if (task.reservations.truncated) {
      truncated = true;
    }
  }
  if (maxCosts.length > 0) {
    maxMicro = sumMicro(maxCosts);
  }
  return {
    tokens: usage.providerCalls.tokens,
    spentMicro: microToString(sumMicro(spentCosts)),
    maxMicro: maxMicro === null ? null : microToString(maxMicro),
    openMicro: microToString(sumMicro(openCosts)),
    truncated,
  };
}

function runSummary(run: NativeTaskRun): RunSummary {
  return {
    taskId: String(run.task_id),
    runId: run.run_id,
    mode: run.mode,
    state: run.state,
    goal: run.goal,
    model: run.model,
  };
}

// ---------------------------------------------------------------- task actions

/** One synthetic pending envelope for non-webview starts (command path). */
function pendingEnvelope(text: string): PendingSubmission {
  return {
    text,
    sessionId: null,
    draftId: null,
    messageId: null,
    files: [],
    attachments: [],
  };
}

// ------------------------------------------------- composer attachments

/** The advertised admission policy of the session's chosen model, else the
 *  conservative emergency ceiling (only when bytes are actually attached). */
async function composerAttachmentPolicy(): Promise<AttachmentAdmissionPolicy> {
  const client = active.client;
  const session = store.snapshot().session;
  if (!client || session === null) {
    return EMERGENCY_ATTACHMENT_POLICY;
  }
  try {
    const catalog = await client.modelCatalog();
    return attachmentPolicyForModel(catalog, session.provider, session.model);
  } catch {
    return EMERGENCY_ATTACHMENT_POLICY;
  }
}

/** Post the bounded metadata list (never bytes) with its refusal display. */
function postComposerAttachments(policy: AttachmentAdmissionPolicy): void {
  chatProvider?.postAttachments(
    composerAttachments.list().map((entry) => ({
      ...entry,
      refusal: composerAttachmentRefusal(entry, policy),
    })),
  );
}

/**
 * Re-post host-owned view state after a (re)resolution: the bounded
 * attachment metadata (with refusals) and the current stream-block reason.
 * Neither travels in the durable snapshot, so without this a closed and
 * reopened Faktor view shows an empty attachment list and a generic stream
 * reason while the host still holds the bytes and the typed refusal.
 */
async function postHostOwnedViewState(): Promise<void> {
  postComposerAttachments(await composerAttachmentPolicy());
  const blocked = active.stream?.blocked ?? null;
  if (blocked !== null) {
    chatProvider?.postStreamBlocked(blocked.reason, blocked.cursor);
  }
}

/**
 * The Attach button: the HOST opens the native picker and reads the chosen
 * files itself (bounded by the 7 MiB upload contract and the store's total
 * bound), so no picker bytes transit the webview. The panel only ever sees
 * metadata ids, filename/mime/size and the refusal reason.
 */
async function pickComposerAttachments(): Promise<void> {
  const selection = await vscode.window.showOpenDialog({
    canSelectMany: true,
    title: 'Faktor: attach files',
    openLabel: 'Attach',
  });
  if (!selection || selection.length === 0) {
    return;
  }
  const policy = await composerAttachmentPolicy();
  const refused: string[] = [];
  for (const uri of selection) {
    if (composerAttachments.size() >= MAX_COMPOSER_ATTACHMENTS) {
      refused.push(`at most ${MAX_COMPOSER_ATTACHMENTS} attachments per submission`);
      break;
    }
    const filename = basename(uri.fsPath);
    try {
      const stat = await vscode.workspace.fs.stat(uri);
      if (stat.size > MAX_PENDING_ATTACHMENT_BYTES) {
        refused.push(
          `${filename}: ${stat.size} bytes exceeds the ${MAX_PENDING_ATTACHMENT_BYTES} byte upload bound`,
        );
        continue;
      }
      const data = await vscode.workspace.fs.readFile(uri);
      if (data.byteLength > MAX_PENDING_ATTACHMENT_BYTES) {
        refused.push(
          `${filename}: ${data.byteLength} bytes exceeds the ${MAX_PENDING_ATTACHMENT_BYTES} byte upload bound`,
        );
        continue;
      }
      const mime = composerMimeForFilename(filename);
      const added = composerAttachments.add({
        filename,
        mime,
        bytes: data.byteLength,
        dataBase64: Buffer.from(data).toString('base64'),
        isImage: mime.startsWith('image/'),
      });
      if (added.reason !== null) {
        refused.push(`${filename}: ${added.reason}`);
      }
    } catch (error) {
      refused.push(`${filename}: ${messageOf(error)}`);
    }
  }
  postComposerAttachments(policy);
  if (refused.length > 0) {
    chatProvider?.postUserError(attachmentRefusalProjection(refused));
  }
}

/**
 * Paste/drop ingestion: the ONE path where bytes originate in the webview
 * (a clipboard image or a dropped File has no host path). The host
 * re-validates the strict bounded shape and re-bounds it again before the
 * bytes enter the store; everything after this point is id-based.
 */
async function addComposerAttachmentData(message: ChatMessage): Promise<void> {
  const items = message.items;
  if (!Array.isArray(items) || items.length === 0) {
    return;
  }
  const policy = await composerAttachmentPolicy();
  const refused: string[] = [];
  for (const item of items.slice(0, MAX_COMPOSER_ATTACHMENTS)) {
    const name =
      typeof item === 'object' &&
      item !== null &&
      typeof (item as { filename?: unknown }).filename === 'string'
        ? (item as { filename: string }).filename
        : 'attachment';
    const added = composerAttachments.add(item);
    if (added.reason !== null) {
      refused.push(`${name}: ${added.reason}`);
    }
  }
  if (items.length > MAX_COMPOSER_ATTACHMENTS) {
    refused.push(`only the first ${MAX_COMPOSER_ATTACHMENTS} items were kept`);
  }
  postComposerAttachments(policy);
  if (refused.length > 0) {
    chatProvider?.postUserError(attachmentRefusalProjection(refused));
  }
}

async function removeComposerAttachment(message: ChatMessage): Promise<void> {
  // An in-flight submission owns its immutable snapshot; visible mutations
  // are ignored until its explicit result arrives.
  if (taskStartGate.inFlight()) {
    return;
  }
  if (composerAttachments.remove(message.id)) {
    postComposerAttachments(await composerAttachmentPolicy());
  }
}

async function clearComposerAttachments(): Promise<void> {
  if (taskStartGate.inFlight()) {
    return;
  }
  composerAttachments.clear();
  postComposerAttachments(await composerAttachmentPolicy());
}

/** The explicit post-upgrade stream recovery (never an automatic loop). */
function recoverEventStream(): void {
  const stream = active.stream;
  const blocked = stream?.blocked ?? null;
  if (!stream || blocked === null) {
    chatProvider?.postNotice('info', 'the event stream is not blocked');
    return;
  }
  // T27: the explicit reconnect is now in flight; the last valid cursor is
  // kept and the formerly offending frame is replayed, never skipped.
  chatProvider?.postStreamRecovery(true);
  stream.recover();
  store.patch({ streamStatus: 'connecting' });
  // The stream recovery retires its OWN paused-stream error; a turn failure
  // recorded earlier is never cleared by a service recovery (item 11).
  clearServiceScopedError();
  chatProvider?.postStreamBlocked(null);
  chatProvider?.postNotice(
    'info',
    'reconnecting live updates from the paused position; the blocked event is replayed, never skipped',
  );
  scheduleRefresh(0);
}

/** The cause-specific attention kind for a failed Ask turn. */
function askErrorKind(error: unknown): UserErrorKind {
  const cause = classifyAskFailure(error);
  if (cause === 'transport') {
    return 'daemon_unreachable';
  }
  if (cause === 'stale_session') {
    return 'conversation_missing';
  }
  return 'run_failure';
}

/**
 * The live Ask connection: service auto-start is AWAITED, then the workspace
 * session is resolved/created. A missing client after a successful start is
 * itself a transport failure, never a generic error.
 */
async function ensureAskConnection(
  context: vscode.ExtensionContext,
): Promise<AskRecoveryConnection> {
  if (!active.client || !active.sessionId) {
    await startServer(context);
  }
  const client = active.client;
  if (!client) {
    throw new NativeTransportError(
      'POST /native/session/{id}/prompt',
      'the Faktor service is not running; the message was kept',
    );
  }
  const sessionId = await ensureSession(client, context);
  return { client, sessionId };
}

/**
 * A transport failure means the supervised connection's base URL/port may be
 * stale. Reconnect NOW (the supervisor replaces the client and refreshes the
 * address), then resolve the session on the new connection.
 */
async function recoverAskTransport(
  context: vscode.ExtensionContext,
): Promise<AskRecoveryConnection> {
  await ensureServiceLifecycle(context).reconnectNow();
  const client = active.client;
  if (!client) {
    throw new NativeTransportError(
      'POST /native/session/{id}/prompt',
      'the Faktor service could not be reconnected; the message was kept',
    );
  }
  const sessionId = await ensureSession(client, context);
  return { client, sessionId };
}

/** A typed 404 rebinds/creates the workspace session on the same client. */
async function rebindAskSession(
  connection: AskRecoveryConnection,
  context: vscode.ExtensionContext,
): Promise<AskRecoveryConnection> {
  const sessionId = await ensureSession(connection.client as NativeClient, context, {
    force: true,
  });
  return { client: connection.client, sessionId };
}

/**
 * ONE plain conversational turn. Ask is not a task: this path posts only the
 * ordinary prompt (plus workspace-relative files) and reads conversation
 * data. It never mints a contract, task-run object, proof read, verification
 * gate or billing/control-plane request. The host retries at most once after
 * refreshing a stale connection or rebinding a stale session.
 */
async function sendAskTurnFromComposer(
  message: ChatMessage,
  goal: string,
  context: vscode.ExtensionContext,
): Promise<void> {
  // Ack fencing (audit item 7): the submission is bound to ONE conversation
  // key (the bound session, or `workspace` before the first resolve). A
  // second send to the SAME conversation while one is in flight is ignored;
  // a send to a DIFFERENT conversation is admitted (its own key is free).
  // The lifecycle generation, the conversation and the logical submission
  // id are captured HERE: every receipt/error below is routed back through
  // them, so switching conversations while a reply runs never retargets the
  // visible one.
  const conversationKey = active.sessionId ?? 'workspace';
  for (const [, entry] of active.askInFlight) {
    if (entry.conversationKey === conversationKey && entry.viewEpoch === conversationViewEpoch) {
      return;
    }
  }
  const panelToken =
    typeof message.submissionToken === 'string' && message.submissionToken.length <= 64
      ? message.submissionToken
      : null;
  const { files, refused } = boundedWebviewFiles(message.files);
  if (refused.length > 0) {
    const reasons = refused.map((entry) => `#${entry.index}: ${entry.reason}`);
    chatProvider?.postUserError(
      attachmentRefusalProjection(reasons, {
        summary: 'Some files were left out',
        hint: 'The message uses the accepted files; add the refused ones and try again if they matter.',
      }),
    );
  }
  const generation = lifecycle.current();
  const viewEpoch = conversationViewEpoch;
  const submissionId = randomUUID();
  const flight: AskInFlightEntry = {
    generation,
    viewEpoch,
    conversationKey,
    sessionId: active.sessionId,
    submissionId,
    panelToken,
    goal,
  };
  active.askInFlight.set(submissionId, flight);
  // ONE turn reducer: idle → submitting. Busy follows the reducer from
  // here on (never the HTTP status line).
  dispatchTurn({ type: 'submit', source: 'ask' });
  const correlation = (): { sessionId: string | null; submissionToken: string | null } => ({
    sessionId: flight.sessionId,
    submissionToken: panelToken,
  });
  /**
   * TRUE while the operator still views the conversation this send bound.
   * Only an explicit conversation switch moves the view epoch; a daemon
   * session rebind inside the SAME conversation (stale-session recovery)
   * never does, so it cannot fake a stale receipt.
   */
  const stillVisible = (): boolean => conversationViewEpoch === viewEpoch;
  try {
    // Subscribe-before-submit: the live-output handshake must be open
    // BEFORE the prompt POST so a fast reply cannot outrun the listener.
    // Live output is best-effort — a failed subscription degrades to the
    // existing page refresh, it never fails the turn.
    try {
      await ensureAskConnection(context);
      startLiveOutput();
    } catch (error) {
      recordBackgroundError(error);
    }
    const outcome = await sendAskTurnWithRecovery(
      {
        ensureConnection: () => ensureAskConnection(context),
        recoverTransport: () => recoverAskTransport(context),
        rebindSession: (connection) => rebindAskSession(connection, context),
      },
      {
        prompt: goal,
        files,
        // Stable across the one retry: the daemon's prompt admission replays
        // an accepted receipt byte-for-byte instead of duplicating the turn.
        submissionId,
      },
    );
    flight.sessionId = outcome.connection.sessionId;
    if (!stillVisible()) {
      // The operator switched away while this reply ran: the receipt lands
      // on ITS OWN conversation only. The visible conversation's turn
      // lifecycle, busy flag and transcript are never retargeted; the panel
      // receives the correlated result and fences its composer by token.
      chatProvider?.postStartResult(goal, true, false, correlation());
      return;
    }
    active.sessionId = outcome.connection.sessionId;
    // Ask owns the conversation from here: subsequent stream-driven
    // refreshes read conversation state only until an explicit Task start.
    active.askOnly = true;
    active.activeRunId = null;
    active.completionContract = null;
    // The receipt's operation/execution ids are stored as INTERNAL
    // correlation (queued → `queued`, accepted → `starting`). HTTP
    // acceptance is admission only: it never completes a turn and never
    // clears busy while the reply is still generating.
    dispatchTurn({
      type: 'receipt',
      opId: outcome.receipt.op_id,
      runId: outcome.receipt.run_id,
      queued: outcome.receipt.queued === true,
    });
    store.patch({
      activeRunId: null,
      busy: turnLifecycleIsLive(turns) || active.activeRunId !== null,
      lastError: null,
      lastErrorKind: null,
    });
    chatProvider?.postStartResult(goal, true, false, correlation());
    await refreshConversation();
  } catch (error) {
    const retryable = error instanceof NativeApiError ? error.retryable : true;
    const reason = messageOf(error);
    if (stillVisible()) {
      // The submission never landed (or its response was lost): the tracked
      // entry ends with its exact outcome — busy must not stick on a turn
      // the daemon never admitted. A retry starts a fresh entry.
      dispatchTurn({ type: 'turnStatus', opId: null, status: 'failed' });
      store.patch({ lastError: reason, lastErrorKind: 'run_failure' });
      reportError(error, askErrorKind(error));
    }
    // The correlated result is posted on EVERY path (including a stale
    // conversation) so the panel can release exactly its own token lock;
    // the visible conversation is only touched when still current.
    chatProvider?.postStartResult(goal, false, retryable, correlation());
  } finally {
    active.askInFlight.delete(submissionId);
  }
}

async function startTask(
  snapshot: StartSubmissionSnapshot,
  context: vscode.ExtensionContext,
  correlation?: { readonly sessionId: string | null; readonly submissionToken: string | null },
): Promise<void> {
  const { submissionId, pending, files, contract, contractRows } = snapshot;
  const goal = pending.text;
  // Ack fencing (audit item 7): the start is bound to the conversation the
  // operator was viewing when it was admitted; a switch mid-start never
  // retargets the visible conversation's run state or composer.
  const viewEpoch = conversationViewEpoch;
  const stillVisible = (): boolean => conversationViewEpoch === viewEpoch;
  // An explicit Task start leaves the conversational-only refresh mode.
  active.askOnly = false;
  const restore = (failure: AdmitFailure, enriched: PendingSubmission): void => {
    // Audit 29: the enriched envelope keeps every successful upload, so a
    // retry resolves the durable ids first and uploads only absent
    // attachments. ONLY a durable acceptance may release the retained state.
    pendingSubmissionRetainer.retain(enriched);
    reportError(new Error(failure.message), 'run_failure');
    // Audit 32: a failure that may have left a durable receipt (status null,
    // 5xx, 408/429, or the daemon's "submission id still in flight" 409)
    // keeps the logical submission retryable under the SAME submission id;
    // an explicit 4xx refusal ends it.
    const retryable = submissionRetryable(failure);
    taskStartGate.settle(submissionId, retryable ? 'transport' : 'refused');
    // NEVER clear before acceptance: the restore callback carries the
    // original text back to the composer and rebuilds the draft. The
    // retryable classification rides along so the composer shows T08
    // `Start response unavailable` only when the retry is the SAME logical
    // submission.
    chatProvider?.postSendMessageFailed(enriched, failure.message, retryable, correlation);
  };
  const refuse = (message: string, enriched: PendingSubmission): void => {
    pendingSubmissionRetainer.retain(enriched);
    reportError(new Error(message), 'run_failure');
    taskStartGate.settle(submissionId, 'refused');
    chatProvider?.postSendMessageFailed(enriched, message, false, correlation);
  };
  try {
    if (!active.client || !active.sessionId) {
      await startServer(context);
    }
    const client = active.client;
    const sessionId = active.sessionId;
    if (!client || !sessionId) {
      restore(
        {
          kind: 'transport',
          stage: 'start',
          status: null,
          code: null,
          message: 'daemon/session unavailable; the draft and attachments were kept',
        },
        pending,
      );
      return;
    }
    // Attachment admission numbers come from the daemon's advertised model
    // contract, never from client-side mirrors: fetch the catalog for the
    // session's chosen model; on any read failure fall back to the
    // conservative emergency ceiling. Only done when bytes are attached.
    let attachmentLimits: AttachmentAdmissionPolicy | undefined;
    if (pending.attachments.length > 0) {
      const session = store.snapshot().session;
      if (session !== null) {
        try {
          const catalog = await client.modelCatalog();
          attachmentLimits = attachmentPolicyForModel(catalog, session.provider, session.model);
        } catch {
          attachmentLimits = EMERGENCY_ATTACHMENT_POLICY;
        }
      } else {
        attachmentLimits = EMERGENCY_ATTACHMENT_POLICY;
      }
    }
    // I02: the captured per-run budget override wins per dimension for THIS
    // submission; the settings remain the fallback and keep their exact
    // validation (never a silently rounded budget). With an override cost,
    // the settings value is not consulted at all.
    const budgetCostRaw = config('budgetCostMicro', 0);
    const settingsCostMicro = microFromNumber(budgetCostRaw);
    const effectiveBudget = resolveRunBudget({
      override: snapshot.budget,
      settingsTokens: config('budgetTokens', 0),
      settingsCostMicro,
    });
    if (effectiveBudget.maxCostMicro === null) {
      // A configured micro amount above 2^53-1 cannot be represented exactly
      // by a VS Code number setting; refusing is the only honest option (the
      // run must never start with a silently rounded budget).
      refuse(
        `faktor.budgetCostMicro ${budgetCostRaw} is not an exact non-negative integer micro amount (max 2^53-1); the task was not started`,
        pending,
      );
      return;
    }
    const settings: StartTaskSettings = {
      // Shadow-only: empty (the default) inherits the daemon's sole mode and
      // "shadow" names it explicitly. The removed direct_compat value is a
      // typed refusal at admission; a refused shadow run never downgrades.
      mutationMode: config('mutationMode', ''),
      maxTokens: effectiveBudget.maxTokens,
      maxCostMicro: effectiveBudget.maxCostMicro,
      files,
      completionContract: contract,
      contractRows,
      submissionId,
      // Item 13: the operator's explicit per-conversation model choice
      // rides the daemon's typed per-run `model` override.
      model: active.futureModelOverride,
    };
    const outcome = await admitPendingSubmission({
      client,
      sessionId,
      pending,
      settings,
      attachmentLimits,
      onStarted: (started) => {
        if (!stillVisible()) {
          return;
        }
        active.activeRunId = started.run_id;
        active.completionContract = contract;
        store.patch({ activeRunId: started.run_id, busy: true, lastError: null, lastErrorKind: null });
        const attached = files.length + pending.attachments.length;
        const attachments = attached > 0 ? ` with ${attached} attachment${attached === 1 ? '' : 's'}` : '';
        const steps = contract !== null ? ' + completion contract' : '';
        chatProvider?.postNotice(
          'info',
          `task run ${started.run_id} started (${started.state})${attachments}${steps}`,
        );
        scheduleRefresh(0);
      },
      onFailure: () => {
        // `restore` is the single terminal callback and owns the visible
        // report (it posts the error and retains the retryable envelope);
        // reporting here too produced two toasts/notices per failure.
      },
      restore,
    });
    if (outcome.ok) {
      // Durable acceptance: ONLY now may the pending envelope (and its
      // retained uploads) be dropped. Clear EXACTLY the host-side entries
      // this snapshot selected — an attachment added for the next task while
      // the request was in flight keeps its id and survives (audit P1-VSCODE).
      pendingSubmissionRetainer.release(outcome.pending);
      taskStartGate.settle(submissionId, 'started');
      if (snapshot.attachmentIds.length > 0) {
        const removed = composerAttachments.removeMany(snapshot.attachmentIds);
        if (removed > 0) {
          postComposerAttachments(await composerAttachmentPolicy());
        }
      } else if (snapshot.pending.attachments.length > 0) {
        // Legacy raw envelope: the submitted bytes were never host-store
        // entries, so the store is untouched; only the submitted display is
        // cleared.
        chatProvider?.postAttachmentsCleared();
      }
      chatProvider?.postStartResult(goal, true, false, correlation);
    }
  } catch (error) {
    if (stillVisible()) {
      reportError(error);
    }
    taskStartGate.settle(submissionId, 'transport');
    chatProvider?.postSendMessageFailed(
      pending,
      error instanceof Error ? error.message : String(error),
      true,
      correlation,
    );
  }
}

/**
 * Task-mode completion controls for the command path (the chat composer
 * carries the same three steps as one ordered finish-level radio group): a
 * multi-select list of the three conditional steps. An explicit empty
 * selection = today's default path (no contract, no work item); a DISMISSED
 * picker aborts the flow (the caller must not start the run the operator
 * just cancelled).
 */
async function promptCompletionContract(): Promise<
  NativeCompletionContract | null | undefined
> {
  const picks = await vscode.window.showQuickPick(
    [
      { label: 'Commit when verified', key: 'include_commit' as const, picked: false },
      { label: 'Push', key: 'include_push' as const, picked: false },
      { label: 'Create PR', key: 'include_pr' as const, picked: false },
    ],
    {
      canPickMany: true,
      title: 'Faktor: Task completion contract',
      placeHolder: 'Conditional steps the durable completion gate must certify (Task mode only)',
      ignoreFocusOut: true,
    },
  );
  return completionContractFromPicks(picks);
}

async function newTaskFromCommand(context: vscode.ExtensionContext): Promise<void> {
  const goal = await vscode.window.showInputBox({
    title: 'Faktor: new task',
    prompt: 'Goal for the task run',
    ignoreFocusOut: true,
  });
  if (goal === undefined || goal.trim().length === 0) {
    return;
  }
  const contract = await promptCompletionContract();
  if (contract === undefined) {
    // The operator dismissed the completion-contract step: abort the start
    // instead of silently running with the default path.
    return;
  }
  const pending = pendingEnvelope(goal.trim());
  const decision = taskStartGate.admit({
    pending,
    files: [],
    contract,
    // The command path authors no contract rows; the composer disclosure
    // is the only surface that carries them.
    contractRows: null,
    newId: randomUUID,
  });
  if (decision.action !== 'start') {
    chatProvider?.postNotice(
      'error',
      decision.action === 'busy'
        ? decision.reason
        : 'a task start is already in flight; the duplicate command was ignored',
    );
    return;
  }
  await startTask(decision.snapshot, context);
}

async function cancelActiveRun(): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  // Target ONLY an active (non-terminal) run: a terminal run in the list is
  // not cancellable and the server's typed 409 is never provoked.
  const runId = cancelRunTarget(active.activeRunId, store.snapshot().runs);
  if (!client || !sessionId) {
    chatProvider?.postNotice('info', 'no active session to stop');
    return;
  }
  if (runId !== null) {
    try {
      const ack = await client.cancelTaskRun(sessionId, runId);
      chatProvider?.postNotice('info', `run ${ack.run_id} cancel requested`);
      active.activeRunId = null;
      store.patch({ activeRunId: null, busy: turnLifecycleIsLive(turns) });
      scheduleRefresh(0);
    } catch (error) {
      reportError(error);
    }
    return;
  }
  // Ask turn: abort EXACTLY the tracked operation through the typed native
  // abort capability — never an unrelated task cancel, never a guess.
  const opId = turnStopOpId(turns);
  if (opId === null) {
    chatProvider?.postNotice('info', 'no active turn to stop');
    return;
  }
  try {
    const ack = await client.abortSession(sessionId, opId);
    if (ack.aborted.indexOf(opId) >= 0) {
      // The daemon confirms IT cancelled this operation: the exact outcome
      // label (partial reply kept, phase `interrupted`).
      dispatchTurn({ type: 'stopped', opId });
      chatProvider?.postNotice('info', 'turn stopped');
    } else {
      chatProvider?.postNotice(
        'error',
        'the daemon did not cancel this turn; its authoritative status will follow',
      );
    }
    scheduleRefresh(0);
  } catch (error) {
    reportError(error);
  }
}

/**
 * The single permission-reply path used by BOTH the approval card (webview)
 * and the `faktor.replyPermission` quick pick: the strict reply body carries
 * the OWNING session id from the selected entry — never a guessed session.
 * Typed 409 refusals surface as the structured "no longer live" projection
 * (`unknown_or_resolved` vs `session_mismatch` demoted to the technical
 * disclosure) and are NEVER retried blindly; the pending list is refreshed
 * either way so the operator works from the service's current truth.
 */
async function replyPermissionEntry(
  sessionId: string,
  permissionId: string,
  decision: 'allow' | 'deny',
): Promise<void> {
  // The host re-checks the rendered pending set before forwarding a decision:
  // a stale/forged webview id, or an id owned by another session, is refused
  // locally and never reaches the daemon. The daemon still enforces the live
  // waiter + owning session as the final authority.
  const pending = store.snapshot().permissions.find((entry) => entry.id === permissionId);
  if (!pending || pending.sessionId !== sessionId) {
    chatProvider?.postPermissionResult(
      permissionId,
      false,
      'this request is not in the current pending list; refresh and review the exact request',
    );
    return;
  }
  const client = active.client;
  if (!client) {
    const reason = 'no active service connection';
    recordError(projectOf(new Error(reason), 'daemon_down'));
    chatProvider?.postPermissionResult(permissionId, false, reason);
    return;
  }
  try {
    await client.replyPermission(sessionId, permissionId, decision);
    chatProvider?.postPermissionResult(permissionId, true);
    chatProvider?.postNotice('info', `permission #${permissionId}: ${decision}`);
    scheduleRefresh(0);
  } catch (error) {
    const failure = classifyPermissionReplyFailure(error);
    if (failure === null) {
      chatProvider?.postPermissionResult(permissionId, false, messageOf(error));
      reportError(error);
      return;
    }
    const projection = projectError({
      kind: 'permission_expired',
      message: permissionReplyFailureMessage(failure, permissionId),
      code: errorCodeOf(error),
    });
    chatProvider?.postPermissionResult(
      permissionId,
      false,
      permissionReplyFailureMessage(failure, permissionId),
    );
    recordError(projection, true);
    scheduleRefresh(0);
  }
}

/**
 * Reply to ONE live pending permission of the active session
 * (`faktor.replyPermission`). The picker lists exactly the daemon's pending
 * set; the reply itself is the shared `replyPermissionEntry` path.
 */
async function replyPermissionFromCommand(): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  if (!client || !sessionId) {
    chatProvider?.postNotice('info', 'start the Faktor service and open a conversation first');
    return;
  }
  let permissions: NativePermissionEntry[];
  try {
    permissions = await client.permissions(sessionId);
  } catch (error) {
    reportError(error);
    return;
  }
  if (permissions.length === 0) {
    chatProvider?.postNotice('info', 'no pending permission requests for this session');
    return;
  }
  const picked = await vscode.window.showQuickPick(
    permissions.map((permission) => ({
      label: `#${permission.id} ${permission.capability}`,
      description: `session ${permission.sessionId}`,
      detail: permission.detail.length > 200 ? `${permission.detail.slice(0, 200)}…` : permission.detail,
      permission,
    })),
    {
      title: 'Faktor: reply to a pending permission',
      placeHolder: 'Select the request to resolve',
      ignoreFocusOut: true,
    },
  );
  if (picked === undefined) {
    return;
  }
  const decision = await vscode.window.showQuickPick(
    [
      { label: 'Allow', value: 'allow' as const },
      { label: 'Deny', value: 'deny' as const },
    ],
    {
      title: `Faktor: permission #${picked.permission.id} (${picked.permission.capability})`,
      placeHolder: 'Decision sent to the owning session',
      ignoreFocusOut: true,
    },
  );
  if (decision === undefined) {
    return;
  }
  await replyPermissionEntry(picked.permission.sessionId, picked.permission.id, decision.value);
}

/**
 * Decide/abort of the tracked durable tournament, state-gated by the SAME
 * cockpit rule the UI renders (`canDecide` / `open`). The server remains the
 * authority: a non-open tournament or no eligible winner surfaces as a typed
 * `NativeApiError`, never a silent no-op.
 */
async function controlTournament(message: ChatMessage): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  const tournamentId =
    typeof message.tournamentId === 'string' ? message.tournamentId.trim() : '';
  const action = typeof message.action === 'string' ? message.action : '';
  if (!client || !sessionId || tournamentId.length === 0) {
    return;
  }
  const view = store.snapshot().tournament;
  if (view !== null && view.id === tournamentId) {
    if (action === 'decide' && !view.canDecide) {
      chatProvider?.postNotice('info', `tournament ${tournamentId} cannot decide yet`);
      return;
    }
    if (action === 'abort' && !view.open) {
      chatProvider?.postNotice('info', `tournament ${tournamentId} is no longer open`);
      return;
    }
  }
  try {
    if (action === 'decide') {
      const decision = await client.decideTournament(sessionId, tournamentId);
      chatProvider?.postNotice(
        'info',
        `tournament ${decision.tournamentId}: winner ${decision.winner} proposed (integration stays the approved-merge path)`,
      );
    } else if (action === 'abort') {
      const reason = typeof message.reason === 'string' ? message.reason.trim() : '';
      await client.abortTournament(sessionId, tournamentId, reason.length > 0 ? reason : undefined);
      chatProvider?.postNotice('info', `tournament ${tournamentId} aborted`);
    } else {
      return;
    }
    scheduleRefresh(0);
  } catch (error) {
    reportError(error);
  }
}

/**
 * The usage/credits panel controls. `usage-next`/`usage-prev` move the
 * organization-wide event cursor (a stack keeps Previous exact); the
 * `grant-credits` key is refused unless the identity carries the
 * `credits_grant` capability — the server role check stays the gate.
 */
async function controlUsage(message: ChatMessage): Promise<void> {
  const action = typeof message.action === 'string' ? message.action : '';
  const panel = store.snapshot().usagePanel;
  if (action === 'usage-next') {
    const next = panel?.page.nextCursor ?? null;
    if (next === null) {
      chatProvider?.postNotice('info', 'no further usage page is served');
      return;
    }
    active.billingPrevCursors.push(active.billingCursor ?? '');
    active.billingCursor = next;
    await readAccountSurface();
    return;
  }
  if (action === 'usage-prev') {
    if (active.billingPrevCursors.length === 0) {
      chatProvider?.postNotice('info', 'already at the first usage page');
      return;
    }
    const previous = active.billingPrevCursors.pop() ?? '';
    active.billingCursor = previous.length > 0 ? previous : null;
    await readAccountSurface();
    return;
  }
  if (action === 'grant-credits') {
    await grantCreditsFromCommand();
    return;
  }
  if (action === 'usage-export') {
    await exportUsageFromCommand();
    return;
  }
  if (action === 'change-plan') {
    await changePlanFromCommand();
    return;
  }
  if (action === 'cancel-subscription') {
    await cancelSubscriptionFromCommand();
    return;
  }
}

/** The bounded usage download: one JSON page set, saved through the host. */
async function exportUsageFromCommand(): Promise<void> {
  const client = active.client;
  const organization = active.billingIdentity?.organization;
  if (!client || organization === undefined) {
    chatProvider?.postNotice('error', 'usage export requires a control-plane organization');
    return;
  }
  const target = await vscode.window.showSaveDialog({
    title: 'Faktor: download usage',
    saveLabel: 'Download usage',
    filters: { JSON: ['json'] },
    defaultUri: vscode.Uri.file(`faktor-usage-${organization}.json`),
  });
  if (target === undefined) {
    return;
  }
  try {
    const page = await client.usageExport(organization);
    const text = JSON.stringify(page, null, 2);
    await vscode.workspace.fs.writeFile(target, Buffer.from(text, 'utf8'));
    chatProvider?.postNotice(
      'info',
      `usage downloaded: ${page.items.length} row(s)${page.truncated ? ' (bounded export; narrow the window with since)' : ''}`,
    );
  } catch (error) {
    reportError(error);
  }
}

/** Owner-only plan change (upgrade/downgrade); proration is typed unavailable. */
async function changePlanFromCommand(): Promise<void> {
  const client = active.client;
  const panel = store.snapshot().usagePanel;
  if (!client) {
    return;
  }
  if (panel === null || !panel.canManageBilling) {
    chatProvider?.postNotice(
      'error',
      panel?.manageDisabledReason ?? 'changing a plan requires an owner control-plane principal',
    );
    return;
  }
  const planId = await vscode.window.showInputBox({
    prompt: 'Plan id (must exist in the [billing] plan table)',
    placeHolder: 'pro',
  });
  if (planId === undefined) {
    return;
  }
  const statusPick = await vscode.window.showQuickPick(
    [
      { label: 'active', description: 'Paid/active plan row' },
      { label: 'trialing', description: 'Time-bounded trial (requires an expiry)' },
    ],
    { title: 'Faktor: plan status', placeHolder: 'active' },
  );
  if (statusPick === undefined) {
    return;
  }
  let expiresMs: number | undefined;
  if (statusPick.label === 'trialing') {
    const raw = await vscode.window.showInputBox({
      prompt: 'Trial end (milliseconds since the Unix epoch)',
      placeHolder: '1800000000000',
      validateInput: (value) =>
        /^[0-9]+$/.test(value.trim()) ? undefined : 'enter a non-negative integer millisecond timestamp',
    });
    if (raw === undefined) {
      return;
    }
    expiresMs = Number(raw.trim());
    if (!Number.isSafeInteger(expiresMs)) {
      void vscode.window.showErrorMessage('refused plan change: the trial end exceeds the safe integer range');
      return;
    }
  }
  const confirm = await vscode.window.showWarningMessage(
    `Change the organization plan to ${planId.trim()} (${statusPick.label})? The change applies immediately; in-flight work is not interrupted.`,
    { modal: true },
    'Change plan',
  );
  if (confirm !== 'Change plan') {
    return;
  }
  try {
    const changed = await client.changeSubscription({
      action: 'change',
      planId: planId.trim(),
      status: statusPick.label === 'trialing' ? 'trialing' : 'active',
      expiresMs,
      idempotencyKey: randomUUID(),
    });
    const proration =
      changed.proration.state === 'available'
        ? 'proration applied'
        : `proration unavailable (${changed.proration.cause})`;
    chatProvider?.postNotice(
      'info',
      changed.replayed
        ? `plan change replayed (idempotent); ${changed.subscription.plan_id} is unchanged`
        : `plan changed to ${changed.subscription.plan_id} (${changed.subscription.status}); ${proration}`,
    );
    await refresh();
  } catch (error) {
    reportError(error);
  }
}

/** Owner-only cancellation; the modal confirmation is the explicit effect. */
async function cancelSubscriptionFromCommand(): Promise<void> {
  const client = active.client;
  const panel = store.snapshot().usagePanel;
  if (!client) {
    return;
  }
  if (panel === null || !panel.canManageBilling) {
    chatProvider?.postNotice(
      'error',
      panel?.manageDisabledReason ?? 'cancelling a subscription requires an owner control-plane principal',
    );
    return;
  }
  const confirm = await vscode.window.showWarningMessage(
    'Cancel the organization subscription? New admissions will be denied immediately; in-flight work is not interrupted.',
    { modal: true },
    'Cancel subscription',
  );
  if (confirm !== 'Cancel subscription') {
    return;
  }
  try {
    const changed = await client.changeSubscription({
      action: 'cancel',
      confirm: true,
      idempotencyKey: randomUUID(),
    });
    chatProvider?.postNotice(
      'info',
      changed.replayed
        ? 'cancellation replayed (idempotent); the subscription is already canceled'
        : 'subscription canceled; new admissions are denied',
    );
    await refresh();
  } catch (error) {
    reportError(error);
  }
}

/** The operator credit grant: amount + reason prompts, idempotency-keyed. */
async function grantCreditsFromCommand(): Promise<void> {
  const client = active.client;
  const panel = store.snapshot().usagePanel;
  if (!client) {
    return;
  }
  if (panel === null || !panel.canGrantCredits) {
    chatProvider?.postNotice(
      'error',
      panel?.grantDisabledReason ?? 'grant credits requires an admin control-plane principal',
    );
    return;
  }
  const amountRaw = await vscode.window.showInputBox({
    prompt: 'Credit grant amount — $5.00 for dollars or 5000000µ$ for microUSD (exact)',
    placeHolder: '$5.00',
    validateInput: (value) => {
      const parsed = parseGrantAmount(value);
      return parsed.ok ? undefined : parsed.reason;
    },
  });
  if (amountRaw === undefined) {
    return;
  }
  const amount = parseGrantAmount(amountRaw);
  if (!amount.ok) {
    reportError(new Error(`refused credit grant: ${amount.reason}`));
    return;
  }
  const reasonRaw = await vscode.window.showInputBox({
    prompt: 'Grant reason (optional)',
    placeHolder: 'operator grant',
  });
  if (reasonRaw === undefined) {
    return;
  }
  const confirmed = await vscode.window.showWarningMessage(
    `Grant $${microUsdText(amount.value)} (${microText(amount.value)}) to ` +
      `${panel.organization ?? 'the organization'}? This is a financial action.`,
    { modal: true },
    'Grant credits',
  );
  if (confirmed !== 'Grant credits') {
    return;
  }
  try {
    const ack = await client.grantCredits({
      amountMicro: amount.value,
      reason: reasonRaw.trim(),
      idempotencyKey: randomUUID(),
    });
    const balance = microBalance(
      ack.credits.granted_micro,
      ack.credits.refunded_micro,
      ack.credits.consumed_micro,
    );
    chatProvider?.postNotice(
      'info',
      ack.duplicate
        ? 'credit grant replayed (idempotent); the recorded grant is unchanged'
        : `credits granted; the balance is now ${microText(balance)}`,
    );
    await refresh();
  } catch (error) {
    reportError(error);
  }
}

/**
 * Parse one operator-entered grant amount exactly. The grant is money: only
 * a plain positive decimal integer within the protocol range is accepted, and
 * the value is kept as a bigint end-to-end (a huge amount is sent as the
 * exact decimal string, never as a rounded number).
 */
function parseGrantAmount(
  raw: string,
): { readonly ok: true; readonly value: MicroMoney } | { readonly ok: false; readonly reason: string } {
  const text = raw.trim();
  // Explicit units only: a bare integer was previously microUSD and is now
  // ambiguous. "$5.00" / "5.00" are dollars; "5000000µ$" is exact micro.
  const microMatch = /^([0-9]+)\s*(?:\u00b5\$|micro|microusd)$/i.exec(text);
  if (microMatch !== null) {
    const value = BigInt(microMatch[1]);
    if (value <= 0n) return { ok: false, reason: 'the grant amount must be greater than zero' };
    if (value > MICRO_I64_MAX) {
      return { ok: false, reason: `the grant amount exceeds the protocol maximum ${MICRO_I64_MAX}` };
    }
    return { ok: true, value };
  }
  const usdMatch = /^\$?([0-9]+)(?:\.([0-9]{1,6}))?$/.exec(text);
  if (usdMatch === null) {
    return { ok: false, reason: 'enter $5.00 for dollars or 5000000µ$ for microUSD' };
  }
  if (usdMatch[2] === undefined && !text.startsWith('$')) {
    return { ok: false, reason: 'add $ for dollars or µ$ for microunits (e.g. $5.00)' };
  }
  const fraction = (usdMatch[2] ?? '').padEnd(6, '0');
  const value = BigInt(usdMatch[1]) * 1_000_000n + BigInt(fraction);
  if (value <= 0n) return { ok: false, reason: 'the grant amount must be greater than zero' };
  if (value > MICRO_I64_MAX) {
    return { ok: false, reason: `the grant amount exceeds the protocol maximum ${MICRO_I64_MAX}` };
  }
  return { ok: true, value };
}

async function controlAgent(message: ChatMessage): Promise<void> {
  const client = active.client;
  const agentId = typeof message.agentId === 'string' ? message.agentId : '';
  const action = typeof message.action === 'string' ? message.action : '';
  if (!client || agentId.length === 0) {
    return;
  }
  try {
    if (action === 'pause') {
      await client.pauseAgent(agentId);
    } else if (action === 'resume') {
      await client.resumeAgent(agentId);
    } else if (action === 'cancel') {
      await client.cancelAgent(agentId);
    } else if (action === 'retry') {
      // Durable Retry: the server is the guard (only Failed children
      // retry; anything else is a typed 409 surfaced by reportError),
      // exactly like the JetBrains client controls.
      await client.retryAgent(agentId);
    } else if (action === 'steer') {
      // Inline note from the companion panel, or the host prompt when the
      // panel/fallback control sends the bare action. BOTH layers run the
      // same non-empty/<=500 guard (the daemon re-validates): a refused
      // value is surfaced TYPED and never sent.
      let note: string | null = null;
      if (typeof message.text === 'string') {
        const guard = normalizeSteerText(message.text);
        if (!guard.ok) {
          reportError(new Error(guard.reason));
          return;
        }
        note = guard.text;
      } else {
        const entered = await vscode.window.showInputBox({
          title: `Faktor: steer ${agentId}`,
          prompt: `Instruction delivered at the agent’s next safe boundary (max ${MAX_STEER_CHARS} chars)`,
          ignoreFocusOut: true,
          validateInput: (value) => {
            const guard = normalizeSteerText(value);
            return guard.ok ? undefined : guard.reason;
          },
        });
        if (entered === undefined) {
          return;
        }
        const guard = normalizeSteerText(entered);
        if (!guard.ok) {
          reportError(new Error(guard.reason));
          return;
        }
        note = guard.text;
      }
      await client.steerAgent(agentId, note);
    } else if (action === 'model') {
      const model = await vscode.window.showInputBox({
        title: `Faktor: model for ${agentId}`,
        prompt: 'Model id from the daemon catalog',
        ignoreFocusOut: true,
      });
      if (model === undefined || model.trim().length === 0) {
        return;
      }
      await client.setAgentModel(agentId, model.trim());
    } else if (action === 'budget') {
      const raw = await vscode.window.showInputBox({
        title: `Faktor: budget for ${agentId}`,
        prompt: 'Max tokens (empty keeps the budget unchanged)',
        ignoreFocusOut: true,
      });
      if (raw === undefined) {
        return;
      }
      const trimmed = raw.trim();
      const tokens = trimmed.length === 0 ? undefined : Number(trimmed);
      if (tokens !== undefined && (!Number.isInteger(tokens) || tokens <= 0)) {
        reportError(new Error('budget must be a positive integer token count'));
        return;
      }
      await client.setAgentBudget(agentId, tokens === undefined ? {} : { max_tokens: tokens });
    } else if (action === 'presentation') {
      // Durable presentation transition (dimmed/tucked background):
      // the server owns the terminal-child refusal (typed 409 surfaced by
      // reportError) and the idempotent same-state no-op.
      const state = message.state;
      if (state !== 'foreground' && state !== 'background') {
        reportError(new Error('presentation state must be foreground or background'));
        return;
      }
      const sessionId = active.sessionId;
      if (!sessionId) {
        return;
      }
      await client.setAgentPresentation(sessionId, agentId, state);
    } else {
      return;
    }
    chatProvider?.postNotice('info', `${action} queued for ${agentId}`);
    scheduleRefresh(0);
  } catch (error) {
    reportError(error);
  }
}

async function retrieveEvidence(message: ChatMessage): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  const evidenceId = message.evidenceId;
  if (!client || !sessionId || typeof evidenceId !== 'number' || !Number.isInteger(evidenceId)) {
    return;
  }
  try {
    const selector: NativeEvidenceSelector = { selector: 'all' };
    const retrieval = await client.retrieveEvidence(sessionId, evidenceId, selector);
    const bytes = Buffer.from(retrieval.bytesBase64, 'base64');
    const truncated =
      retrieval.truncatedByPolicy || bytes.byteLength > MAX_EVIDENCE_PREVIEW_BYTES;
    const text = bytes.subarray(0, MAX_EVIDENCE_PREVIEW_BYTES).toString('utf8');
    chatProvider?.postEvidence(evidenceId, text, truncated);
  } catch (error) {
    reportError(error);
  }
}

// ------------------------------------------------- I03 changes + review

/** The served facts one change read needs (every refusal kept explicit). */
interface ServedChangeContext {
  readonly filesChanged: readonly string[];
  /** The projection read failure: the Changes view is then unavailable. */
  readonly unavailable: string | null;
  readonly verification: NativeTaskVerification | null;
  readonly proof: NativeTaskProof | null;
  readonly candidateDisplay: string;
}

/**
 * Re-run the projection + proof + verification reads (the `Recheck current
 * basis` path, S15) with per-read failure isolation: a projection refusal
 * marks the view unavailable; a proof/verification refusal only degrades the
 * refs (rows still render from the served `filesChanged`).
 */
async function readChangeContext(
  client: NativeClient,
  sessionId: string,
): Promise<ServedChangeContext> {
  const runs = store.snapshot().runs;
  const taskId = runs.length > 0 ? runs[0]!.taskId : null;
  const [projectionRead, verificationRead, proofRead] = await Promise.all([
    client.projection(sessionId).then(
      (value) => ({ value, reason: null as string | null }),
      (error) => ({ value: null as NativeProjection | null, reason: messageOf(error) }),
    ),
    taskId === null
      ? Promise.resolve({ value: null as NativeTaskVerification | null, reason: null })
      : client.taskVerification(sessionId, taskId).then(
          (value) => ({ value, reason: null as string | null }),
          (error) => ({ value: null, reason: messageOf(error) }),
        ),
    taskId === null
      ? Promise.resolve({ value: null as NativeTaskProof | null, reason: null })
      : client.taskProof(sessionId, taskId).then(
          (value) => ({ value, reason: null as string | null }),
          (error) => ({ value: null, reason: messageOf(error) }),
        ),
  ]);
  return {
    filesChanged: projectionRead.value?.filesChanged ?? [],
    unavailable: projectionRead.reason,
    verification: verificationRead.value,
    proof: proofRead.value,
    candidateDisplay: candidateDisplayOf(proofRead.value?.task?.goal ?? null),
  };
}

function changesSummaryOf(context: ServedChangeContext): ReturnType<typeof buildChangesSummary> {
  return buildChangesSummary({
    filesChanged: context.filesChanged,
    verification: context.verification,
    proof: context.proof,
    unavailable: context.unavailable,
    candidateDisplay: context.candidateDisplay,
  });
}

/**
 * `Recheck current basis`: re-run the proof/projection reads and re-derive
 * the conflict facts (S15). There is deliberately NO rebase/apply control:
 * the runtime exposes no such capability, so none is offered.
 */
async function recheckChanges(): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  if (!client || !sessionId) {
    chatProvider?.postNotice('error', 'no active service connection; the current basis was not rechecked');
    return;
  }
  const generation = lifecycle.current();
  const context = await readChangeContext(client, sessionId);
  if (!lifecycle.isCurrent(generation) || active.client !== client || active.sessionId !== sessionId) {
    return;
  }
  if (context.proof !== null) {
    // Keep the strict proof cache coherent with the fresh read, so the
    // Overview and the Changes basis can never disagree after a recheck.
    const runs = store.snapshot().runs;
    const task = store.snapshot().task;
    if (runs.length > 0 && task !== null) {
      active.taskProofKey = `${runs[0]!.taskId}:${task.state}:${runs[0]!.state}`;
      active.taskProof = context.proof;
      active.taskProofUnavailable = null;
    }
  }
  store.patch({ changes: changesSummaryOf(context) });
  chatProvider?.postNotice('info', 'changes rechecked against the served projection and proof');
}

/**
 * The selected-change review adapter: resolve the immutable basis in the
 * documented order and open it in the NATIVE editor as read-only virtual
 * documents. Nothing here reads a workspace file and no Accept/Apply/write
 * action exists. With no served basis the exact `Diff unavailable` in-view
 * state is posted instead.
 */
async function reviewChange(message: ChatMessage): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  const path = typeof message.path === 'string' ? message.path.trim() : '';
  if (!client || !sessionId) {
    chatProvider?.postNotice('error', 'no active service connection; the change was not opened');
    return;
  }
  const generation = lifecycle.current();
  const context = await readChangeContext(client, sessionId);
  if (!lifecycle.isCurrent(generation) || active.client !== client || active.sessionId !== sessionId) {
    return;
  }
  if (context.unavailable !== null) {
    chatProvider?.postNotice('error', `changes are unavailable: ${context.unavailable}`);
    return;
  }
  const rows: readonly ChangeRowSummary[] = changeRowsOf({
    filesChanged: context.filesChanged,
    verification: context.verification,
    proof: context.proof,
  });
  const row = rows.find((entry) => entry.path === path) ?? null;
  if (row === null) {
    chatProvider?.postNotice(
      'error',
      `change ${JSON.stringify(path)} is not among the served changes for this run`,
    );
    return;
  }
  const basis = resolveReviewBasis({
    path,
    row,
    sourceDiffEvidenceId: sourceDiffEvidenceIdOf(context.verification),
    candidateDisplay: context.candidateDisplay,
  });
  const primary = basis.primary ?? basis.secondary;
  if (primary === null) {
    chatProvider?.postChangeReview({ path, state: 'unavailable' });
    return;
  }
  const primaryUri = vscode.Uri.parse(
    evidenceDocumentUri({
      sessionId,
      evidenceId: primary.evidenceId,
      purpose: primary.kind,
      title: primary.title,
    }),
  );
  try {
    const secondary = basis.primary !== null ? basis.secondary : null;
    if (secondary !== null && secondary.evidenceId !== primary.evidenceId) {
      const secondaryUri = vscode.Uri.parse(
        evidenceDocumentUri({
          sessionId,
          evidenceId: secondary.evidenceId,
          purpose: secondary.kind,
          title: secondary.title,
        }),
      );
      await vscode.commands.executeCommand(
        'vscode.diff',
        primaryUri,
        secondaryUri,
        evidenceDiffTitle(primary, secondary),
      );
    } else {
      await vscode.window.showTextDocument(primaryUri, { preview: false });
    }
    chatProvider?.postChangeReview({
      path,
      state: 'opened',
      evidenceId: primary.evidenceId,
    });
  } catch (error) {
    chatProvider?.postChangeReview({
      path,
      state: 'unavailable',
      reason: messageOf(error),
    });
  }
}

// ------------------------------------------------------------ I07 outcomes

/**
 * The value-entitlements status line of the configured organization: the
 * typed report when the daemon can derive it, else the typed refusal
 * (`billing_disabled` renders "billing disabled locally"). Never throws —
 * the outcomes read itself decides the panel state.
 */
async function valueEntitlementsStatusFor(client: NativeClient): Promise<string> {
  try {
    const report = await client.valueEntitlements();
    return valueEntitlementsStatusLine(report.value_entitlements);
  } catch (error) {
    return valueEntitlementsRefusalStatusLine(
      error instanceof NativeApiError ? error.code : null,
      messageOf(error),
    );
  }
}

/**
 * One bounded, explicitly requested outcome-ledger read. The read is
 * LAZY (only the History / Outcomes tab asks for it) and failure-isolated:
 * a refusal patches ONLY the outcomes view to its `Cost unavailable` state
 * and never touches the conversation surface. A cursor read appends the
 * older page; a top read replaces the list.
 *
 * Entitlement surfacing: with a configured control-plane credential the read
 * passes the configured organization (`org=`) — the paid `outcome_analytics`
 * boundary — and the panel carries the value-entitlements status line; a
 * typed 403 `value_entitlement_refused` / 409 `billing_disabled` renders as
 * the refusal (never a fabricated local fallback).
 */
async function readOutcomes(message: ChatMessage): Promise<void> {
  const client = active.client;
  const cursor = parseOutcomesReadRequest(message.beforeMs, message.beforeRun);
  if ('reason' in cursor) {
    chatProvider?.postNotice('error', cursor.reason);
    return;
  }
  if (!client) {
    store.patch({
      outcomes: unavailableOutcomesView('no active service connection', null),
    });
    return;
  }
  const generation = lifecycle.current();
  const org = outcomesOrganizationForRead(
    controlPlaneScope()?.organization ?? null,
    client.hasControlToken(),
  );
  try {
    const entitlementsStatus =
      org === null ? null : await valueEntitlementsStatusFor(client);
    const page = await client.outcomes({
      limit: OUTCOMES_PAGE_LIMIT,
      ...(org !== null ? { org } : {}),
      ...(cursor.beforeMs !== null && cursor.beforeRun !== null
        ? { beforeMs: cursor.beforeMs, beforeRun: cursor.beforeRun }
        : {}),
    });
    if (!lifecycle.isCurrent(generation) || active.client !== client) {
      return;
    }
    const view =
      cursor.beforeMs === null
        ? buildOutcomesView(page, entitlementsStatus)
        : mergeOutcomesPage(store.snapshot().outcomes, page, entitlementsStatus);
    store.patch({ outcomes: view });
  } catch (error) {
    if (!lifecycle.isCurrent(generation) || active.client !== client) {
      return;
    }
    const entitlementsStatus =
      org === null ? null : await valueEntitlementsStatusFor(client);
    store.patch({
      outcomes: unavailableOutcomesView(
        messageOf(error),
        error instanceof NativeApiError ? error.code : null,
        entitlementsStatus,
      ),
    });
  }
}

// --------------------------------------------------------------- status bar

function updateStatusBar(): void {
  if (!statusBar) {
    return;
  }
  const snapshot = store.snapshot();
  // The status bar is product surface: it never names the daemon, a port,
  // readiness or a process. Only the transient connection notice and genuine
  // authority are allowed to interrupt the headline.
  if (snapshot.connectionNotice === 'connecting') {
    statusBar.text = '$(sync~spin) Connecting to Faktor…';
    statusBar.tooltip = 'Opening Faktor';
  } else if (snapshot.connectionNotice === 'reconnecting') {
    statusBar.text = '$(sync~spin) Reconnecting…';
    statusBar.tooltip = 'Restoring the Faktor connection';
  } else if (snapshot.serviceAuthority !== null) {
    statusBar.text = '$(error) Faktor needs attention';
    statusBar.tooltip = snapshot.serviceAuthority.copy;
  } else if (snapshot.task) {
    statusBar.text = `$(sync~spin) Faktor: ${snapshot.task.state}`;
    statusBar.tooltip = 'Faktor is working';
  } else {
    statusBar.text = '$(pulse) Faktor';
    statusBar.tooltip = 'Open Faktor';
  }
  statusBar.show();
}

// ------------------------------------------------------------- webview bridge

async function handleWebviewMessage(
  message: ChatMessage,
  context: vscode.ExtensionContext,
): Promise<void> {
  switch (message.type) {
    case 'ready':
      chatProvider?.postSnapshot(store.snapshot());
      return;
    case 'uiDomDump': {
      // One-shot webview DOM diagnostic (stale/wrong-surface evidence):
      // written host-side so every webview->host message has exactly ONE
      // dispatcher (the registry's message-wiring law).
      const dump = message as { html?: unknown; mode?: unknown; version?: unknown };
      if (typeof dump.html === 'string') {
        try {
          writeFileSync('/tmp/faktor-ui-dump.html', dump.html);
          writeFileSync(
            '/tmp/faktor-ui-meta.json',
            JSON.stringify({
              mode: typeof dump.mode === 'string' ? dump.mode : null,
              version: typeof dump.version === 'string' ? dump.version : null,
              at: Date.now(),
            }),
          );
        } catch {
          /* diagnostic only */
        }
      }
      return;
    }
    case 'startDaemon':
      try {
        await startServer(context);
      } catch (error) {
        reportError(error, 'daemon_start_failed');
      }
      return;
    // T03 `Start anyway`: the card already presented the exact resolved path
    // and the D10 trust wording, so the explicit confirmation is the click
    // (the native modal is the palette path's confirmation).
    case 'confirmStart':
      try {
        await startServer(context, { confirmUnverified: true });
      } catch (error) {
        reportError(error, 'daemon_start_failed');
      }
      return;
    // T05 `Cancel startup` and T03 `Cancel`.
    case 'cancelStart':
      cancelStart();
      return;
    case 'cancelConfirm':
      dismissUnverifiedStart(context);
      return;
    case 'locateDaemon':
      await locateDaemon(context);
      return;
    // T00 `Open project`: the native open-folder gesture; this panel never
    // starts a service against a folderless window.
    case 'openProject':
      await vscode.commands.executeCommand('workbench.action.files.openFolder');
      return;
    // Explicit authority actions (Diagnostics or the normal attention row).
    case 'retryStart':
      try {
        await startServer(context);
      } catch (error) {
        reportError(error, 'daemon_start_failed');
      }
      return;
    case 'reviewTrust':
      await vscode.commands.executeCommand('workbench.action.openSettings', 'faktor.binaryPath');
      return;
    case 'manageWorkspaceTrust':
      await vscode.commands.executeCommand('workbench.trust.manage');
      return;
    case 'stopDaemon':
      stopServer(context);
      return;
    case 'refresh':
      await refreshFromUserGesture();
      return;
    case 'refreshHistory':
      await refreshHistory();
      return;
    // A History row's Open: the ONE explicit switch path. The id always comes
    // from a served listing row; the daemon remains the existence authority.
    case 'openSession': {
      const sessionId = typeof message.sessionId === 'string' ? message.sessionId : '';
      await switchSession(sessionId, context);
      return;
    }
    case 'attachPick':
      await pickComposerAttachments();
      return;
    case 'attachData':
      await addComposerAttachmentData(message);
      return;
    case 'removeAttachment':
      await removeComposerAttachment(message);
      return;
    case 'clearAttachments':
      await clearComposerAttachments();
      return;
    case 'recoverStream':
      recoverEventStream();
      return;
    // The approval card's Allow/Deny: the SAME session-scoped reply path as
    // the palette quick pick, driven by the snapshot's pending list.
    case 'permissionReply': {
      const permissionId = typeof message.permissionId === 'string' ? message.permissionId : '';
      const decision =
        message.decision === 'allow' ? 'allow' : message.decision === 'deny' ? 'deny' : null;
      const sessionId = active.sessionId;
      if (permissionId.length === 0 || decision === null || !sessionId) {
        return;
      }
      await replyPermissionEntry(sessionId, permissionId, decision);
      return;
    }
    case 'loadOlderMessages': {
      // Item 13: explicit scrollback — fetch the OLDER page by the
      // accumulated cursor and MERGE it (never replace); the webview's
      // id-based anchor keeps the reading position stable across the
      // prepend.
      const olderClient = active.client;
      const olderSession = active.sessionId;
      if (
        !olderClient ||
        !olderSession ||
        active.transcriptNextBefore === null ||
        !active.transcriptHasMore ||
        active.loadingOlder
      ) {
        return;
      }
      active.loadingOlder = true;
      const olderGeneration = lifecycle.current();
      try {
        const page = await olderClient.messages(olderSession, {
          before: active.transcriptNextBefore,
          limit: HISTORY_PAGE_LIMIT,
        });
        if (!lifecycle.isCurrent(olderGeneration) || active.sessionId !== olderSession) {
          return;
        }
        const older = transcriptOf(page);
        const previous = store.snapshot();
        const merged = mergeTranscriptPages(previous.transcript, older);
        active.transcriptHasMore = page.hasMore === true;
        active.transcriptNextBefore = page.nextBefore;
        const mergedRevision = transcriptSignature(merged);
        store.patch({
          transcript: merged,
          transcriptRevision: mergedRevision,
          transcriptHasMore: active.transcriptHasMore,
        });
      } catch (error) {
        recordBackgroundError(error);
      } finally {
        if (lifecycle.isCurrent(olderGeneration)) {
          active.loadingOlder = false;
        }
      }
      return;
    }
    case 'sendGoal': {
      const goal = typeof message.goal === 'string' ? message.goal.trim() : '';
      if (goal.length === 0) {
        return;
      }
      // The ONE composer carries an explicit Ask|Task intent. Ask is the
      // default and is a plain conversational turn; it returns BEFORE any
      // task contract, task-run, proof, verification or billing path runs.
      if (composerIntentOf(message.intent) === 'ask') {
        await sendAskTurnFromComposer(message, goal, context);
        return;
      }
      // Host single-flight (audit 32): while one logical submission is in
      // flight, every further sendGoal is ignored — a double click, an
      // Enter+click race and a hostile burst can never admit a second
      // durable prompt/run. After a lost response the gate is retryable and
      // an identical resubmission restarts the SAME immutable snapshot under
      // the host-owned id.
      if (taskStartGate.inFlight()) {
        return;
      }
      // Files ride the workspace-relative attachment vocabulary
      // (`sendGoal.files`); malformed entries are refused individually (with
      // their exact reason) and never discard the goal. The completion
      // contract is Task-mode only: a malformed contract refuses the START
      // loudly — silently starting contract-free would claim a workflow the
      // run never recorded.
      const { files, refused } = boundedWebviewFiles(message.files);
      if (refused.length > 0) {
        const reasons = refused.map((entry) => `#${entry.index}: ${entry.reason}`);
        chatProvider?.postUserError(
          attachmentRefusalProjection(reasons, {
            summary: 'Some files were left out',
            hint: 'The run uses the accepted files; add the refused ones and try again if they matter.',
          }),
        );
      }
      const contract = parseCompletionContract(message.completionContract);
      if ('reason' in contract) {
        chatProvider?.postUserError(
          projectError({ kind: 'run_failure', message: `task start refused: ${contract.reason}` }),
        );
        chatProvider?.postStartResult(goal, false);
        return;
      }
      // The user-authored contract rows are the same class of user intent as
      // the completion contract: a malformed/oversized row refuses the START
      // loudly (silently dropping a stated constraint would claim a contract
      // the run never recorded). An absent value is the default path.
      const contractRows = parseContractRows(message.contractRows);
      if ('reason' in contractRows) {
        chatProvider?.postUserError(
          projectError({
            kind: 'run_failure',
            message: `task start refused: ${contractRows.reason}`,
          }),
        );
        chatProvider?.postStartResult(goal, false);
        return;
      }
      // I02: the bounded per-run budget fields of the "Acceptance & contract"
      // disclosure. A malformed value refuses the START loudly (a typed
      // userError; the draft and the visible options are kept) and never
      // starts a run with a silently dropped or rounded budget.
      const runBudget = parseRunBudget(message.budgetTokens, message.budgetCostMicro);
      if ('reason' in runBudget) {
        chatProvider?.postUserError(
          projectError({ kind: 'run_failure', message: `task start refused: ${runBudget.reason}` }),
        );
        chatProvider?.postStartResult(goal, false);
        return;
      }
      // Binary attachments (pasted/attached images and screenshots) ride the
      // strict pending-submission envelope: the REAL composer path carries
      // HOST-SIDE attachment ids (`sendGoal.attachmentIds`) that resolve into
      // an immutable byte snapshot here; the legacy raw envelope
      // (`sendGoal.attachments`) is still accepted. Either way each entry is
      // re-validated at the host boundary, uploaded to the durable artifact
      // store, and only the typed artifact ids reach the task start. A
      // malformed/unknown envelope refuses the START loudly (the composer
      // draft and its visible attachments are kept) rather than silently
      // dropping bytes the user attached.
      const attachmentIds = message.attachmentIds;
      const binaryAttachments = message.attachments;
      const hostSessionId = active.sessionId ?? message.sessionId;
      let pending: PendingSubmission;
      let selectedAttachmentIds: readonly string[] = [];
      if (attachmentIds !== undefined && attachmentIds !== null) {
        const selected = composerAttachments.select(attachmentIds);
        if (selected.reason !== null) {
          chatProvider?.postUserError(
            attachmentRefusalProjection([`${selected.reason}; the draft was kept`]),
          );
          chatProvider?.postStartResult(goal, false);
          return;
        }
        selectedAttachmentIds = selected.ids;
        const parsed = parsePendingSubmission({
          text: goal,
          sessionId: hostSessionId,
          draftId: message.draftId,
          messageId: hostMessageId(message.messageId),
          files: message.files,
          attachments: selected.attachments,
        });
        if (parsed === null) {
          chatProvider?.postUserError(
            attachmentRefusalProjection([
              'the binary attachment envelope was malformed; the draft was kept',
            ]),
          );
          chatProvider?.postStartResult(goal, false);
          return;
        }
        pending = pendingSubmissionRetainer.restore(parsed);
      } else if (binaryAttachments === undefined || binaryAttachments === null) {
        pending = pendingEnvelope(goal);
      } else {
        const parsed = parsePendingSubmission({
          text: goal,
          sessionId: hostSessionId,
          draftId: message.draftId,
          messageId: hostMessageId(message.messageId),
          files: message.files,
          attachments: binaryAttachments,
        });
        if (parsed === null) {
          chatProvider?.postUserError(
            attachmentRefusalProjection([
              'the binary attachment envelope was malformed; the draft was kept',
            ]),
          );
          chatProvider?.postStartResult(goal, false);
          return;
        }
        pending = pendingSubmissionRetainer.restore(parsed);
      }
      const decision = taskStartGate.admit({
        pending,
        files,
        contract: contract.contract,
        contractRows: contractRows.rows,
        budget: runBudget.budget,
        attachmentIds: selectedAttachmentIds,
        newId: randomUUID,
      });
      if (decision.action !== 'start') {
        // Unreachable after the in-flight guard; a raced gate is still a
        // loud no-op rather than a second start.
        chatProvider?.postUserError(
          projectError({
            kind: 'run_failure',
            message:
              decision.action === 'busy'
                ? decision.reason
                : 'a task start is already in flight; the duplicate submit was ignored',
          }),
        );
        return;
      }
      // A retry restarts the STORED immutable snapshot (never the incoming
      // body), with the retained uploads merged back in so the same bytes
      // are not uploaded twice and the attachment ids stay byte-identical.
      const snapshot = decision.retry
        ? {
            ...decision.snapshot,
            pending: pendingSubmissionRetainer.restore(decision.snapshot.pending),
          }
        : decision.snapshot;
      await startTask(snapshot, context, {
        sessionId: active.sessionId,
        submissionToken:
          typeof message.submissionToken === 'string' && message.submissionToken.length <= 64
            ? message.submissionToken
            : null,
      });
      return;
    }
    case 'cancelRun':
      await cancelActiveRun();
      return;
    case 'liveGap':
      // The panel detected an offset gap in the live frames: fetch the
      // authoritative snapshot and reconcile (never a silent text hole).
      void requestLiveResync(`panel gap: ${String(message.partId ?? '')}`);
      return;
    case 'agentControl':
      await controlAgent(message);
      return;
    case 'tournamentControl':
      await controlTournament(message);
      return;
    case 'usageControl':
      await controlUsage(message);
      return;
    case 'readAccount':
      await readAccountSurface();
      return;
    case 'openProviderSettings':
      await vscode.commands.executeCommand('faktor.openProviders8');
      return;
    case 'openAccountSettings':
      await vscode.commands.executeCommand(
        'workbench.action.openSettings',
        'faktor.controlPlane',
      );
      return;
    case 'uxAction':
      await handleUxAction(message.action);
      return;
    case 'retrieveEvidence':
      await retrieveEvidence(message);
      return;
    // I03: the selected-change review and the explicit basis recheck. Both
    // re-run the served reads; neither writes anything.
    case 'reviewChange':
      await reviewChange(message);
      return;
    case 'recheckChanges':
      await recheckChanges();
      return;
    // I07: the lazily read Engineering Outcome Ledger page (History /
    // Outcomes). The cursor is validated before any read.
    case 'readOutcomes':
      await readOutcomes(message);
      return;
    // `Return to run`: the chat command focuses the panel and the webview's
    // own Work subview switch happens locally (one surface, no round trip
    // needed for the view move).
    case 'openSettingsView':
      // An internal destination of the one panel: no new panel is created.
      openControlCenter(context, 'S29');
      return;
    case 'openWork':
      await vscode.commands.executeCommand('faktor.openChat');
      return;
    // Coordination board: the native surface serves one bounded newest-first
    // page (GET) and one bounded post (POST). The host re-validates every
    // gesture, records an explicit unavailable state for a daemon without
    // the additive route, and never fabricates a post.
    case 'boardRead': {
      const request = parseBoardReadRequest(message.since, message.limit);
      if ('reason' in request) {
        chatProvider?.postNotice('error', request.reason);
        return;
      }
      await readBoard(request.since, request.limit);
      return;
    }
    case 'boardPost':
      await postBoard(message);
      return;
    default:
      return;
  }
}

// ---------------------------------------------------------- Settings surface

/**
 * Settings renders INSIDE the one Faktor sidebar view. There is no settings
 * tab, no editor split and no second surface: the view's internal router
 * swaps Chat / Settings / Inspect / History / Diagnostics in place.
 */
function openControlCenter(_context: vscode.ExtensionContext, screen = 'S29'): void {
  chatProvider?.showSettings(screen, screen === 'S29');
}

// ------------------------------------------------------- Edition 8 surfaces

let edition8Output: vscode.OutputChannel | null = null;

function edition8Deps(context: vscode.ExtensionContext): Edition8PanelDeps {
  if (edition8Output === null) {
    edition8Output = vscode.window.createOutputChannel('Faktor Revision 8');
    context.subscriptions.push(edition8Output);
  }
  const output = edition8Output;
  return {
    backend: () => (active.client === null ? null : new Edition8Client(active.client)),
    session: () => active.sessionId,
    workspace: () => ({
      workspace: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null,
      worktree: null,
      daemon: active.daemon?.baseUrl ?? null,
      generation: null,
    }),
    hostAction: async (controlId: string) => {
      switch (controlId) {
        case 'M-03': {
          const model = vscode.workspace.getConfiguration('faktor').get<string>('defaultModel') ?? 'unselected';
          const added = toggleFavorite(model);
          return `${model} ${added ? 'added to' : 'removed from'} local favorites. This is host MRU state; no daemon write is claimed.`;
        }
        case 'C-04': {
          const editor = vscode.window.activeTextEditor;
          if (editor === undefined) return 'No active editor selection exists; nothing was attached.';
          const selection = editor.selection;
          addContextDraft({
            kind: 'selection',
            uri: editor.document.uri.toString(),
            range: `${selection.start.line}:${selection.start.character}-${selection.end.line}:${selection.end.character}`,
            version: String(editor.document.version),
            reason: 'Current editor selection',
            text: editor.document.getText(selection).slice(0, 32 * 1024),
          });
          return `Captured ${editor.document.uri.fsPath}:${selection.start.line + 1}-${selection.end.line + 1} at version ${editor.document.version}.`;
        }
        case 'X-10': {
          const editor = vscode.window.activeTextEditor;
          if (editor === undefined) return 'No active editor exists; no diagnostics were added.';
          const diagnostics = vscode.languages.getDiagnostics(editor.document.uri);
          addContextDraft({
            kind: 'diagnostic',
            uri: editor.document.uri.toString(),
            range: null,
            version: String(editor.document.version),
            reason: `${diagnostics.length} host diagnostics`,
            text: diagnostics.slice(0, 50).map((entry) => entry.message).join('\n').slice(0, 16 * 1024),
          });
          return `Captured ${diagnostics.length} host diagnostics for the active file.`;
        }
        case 'X-11': {
          const editor = vscode.window.activeTextEditor;
          if (editor === undefined) return 'No active editor exists; no code action was offered.';
          const action = await vscode.window.showQuickPick(
            ['Explain selection', 'Fix selection', 'Test selection', 'Add selection to context'],
            { placeHolder: 'Inline code action (never edits silently)' },
          );
          if (action === undefined) return 'No inline code action was selected.';
          const selection = editor.selection;
          addContextDraft({
            kind: 'selection',
            uri: editor.document.uri.toString(),
            range: `${selection.start.line}:${selection.start.character}-${selection.end.line}:${selection.end.character}`,
            version: String(editor.document.version),
            reason: `Inline code action: ${action}`,
            text: editor.document.getText(selection).slice(0, 32 * 1024),
          });
          if (action !== 'Add selection to context') await vscode.commands.executeCommand('faktor.openChat');
          return `Captured ${editor.document.uri.fsPath}:${selection.start.line + 1}-${selection.end.line + 1} at version ${editor.document.version} for “${action}”; the draft opens without editing the file.`;
        }
        case 'S-05':
          await vscode.commands.executeCommand('workbench.action.showCommands');
          return 'The native command palette is open.';
        case 'D-08':
          return 'Display focus is host-local; choose Compact, Balanced or Verbose in the Conversation settings.';
        case 'D-09':
          await vscode.commands.executeCommand('workbench.action.openSettings', 'window.zoomLevel');
          return 'Theme, zoom and density are host-owned; Faktor never overrides them.';
        case 'D-10':
          await vscode.commands.executeCommand('workbench.action.openSettings', 'faktor.send');
          return 'Enter sends; Shift+Enter inserts a newline; Ctrl/Cmd+Enter also sends.';
        case 'D-11':
          await vscode.commands.executeCommand('workbench.action.showCommands');
          return 'Faktor commands are registered under Faktor in the native palette.';
        case 'D-14':
          await vscode.commands.executeCommand('workbench.action.openSettings', 'accessibility');
          return 'Native accessibility settings are open; reduced motion is honored automatically.';
        case 'D-15':
          await switchSessionFromCommand(context);
          return 'The native conversation switcher finished; each draft stays with its conversation.';
        default:
          return 'Native VS Code action completed.';
      }
    },
    switchSession: (sessionId: string) => switchSession(sessionId, context),
    newConversation: (provider: string, model: string) =>
      newConversationWith(provider, model, context),
    futureModel: async (provider: string, model: string) => {
      if (active.sessionId === null) {
        return 'No conversation is open; open one first, then choose again.';
      }
      active.futureModelOverride = `${provider}/${model}`;
      return (
        `Future Task runs of this conversation use ${provider}/${model} ` +
        '(per-run override on the next start). Ask turns keep the daemon\u2019s ' +
        'routing model until the native prompt route accepts a per-turn model.'
      );
    },
    output,
  };
}

// ------------------------------------------------------------------- lifecycle

export function activate(context: vscode.ExtensionContext): void {
  activeContext = context;
  statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  statusBar.command = 'faktor.openChat';
  context.subscriptions.push(statusBar);
  // Window-level scope label for the Inspect breadcrumb: the workspace NAME
  // (never a raw id). Static for the window's life.
  store.patch({ workspaceName: vscode.workspace.workspaceFolders?.[0]?.name ?? null });
  updateStatusBar();
  // Publish the initial preflight state (workspace presence + machine-scoped
  // resolution + trust): the Work empty state shows exactly this view.
  publishPreflight(context);

  chatProvider = new ChatViewProvider(
    context,
    {
      handle: (message) => handleWebviewMessage(message, context),
      onViewResolved: () => {
        postHostOwnedViewState();
        // Opening Faktor is the ONE automatic trigger: a verified installed
        // executable starts without a Start-service decision. The supervisor
        // dedupes this with command/task triggers.
        void ensureServiceLifecycle(context).open();
      },
    },
    () => edition8Deps(context),
  );

  // EXTERNAL SecretStorage changes (another window, the OS keychain UI, an
  // import) must reach the RUNNING client: `onDidChange` re-runs the same
  // resolution, so an update replaces the live `x-faktor-control-token` and a
  // deletion clears it — no daemon restart, no stale credential.
  const secretWatch = watchControlPlaneSecretChanges({
    secrets: context.secrets,
    apply: async () => {
      const client = active.client;
      if (client === null) {
        return;
      }
      await applyControlPlaneCredential(client, context);
      controlPlaneNotice(
        'info',
        'the control-plane credential in the OS/IDE secret store changed; the running client uses the updated value',
      );
    },
    onError: (error) =>
      controlPlaneNotice('error', `control-plane credential refresh failed: ${messageOf(error)}`),
  });

  context.subscriptions.push(
    // I03: the read-only virtual documents of served evidence. The provider
    // reads ONLY through the native client (never a workspace file), and a
    // typed refusal renders an explicit refusal document.
    registerEvidenceDocumentProvider(() => active.client),
    registerLspDiagnostics(context, {
      client: () => active.client,
      session: () => active.sessionId,
      workspaceRoot: () => vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ?? null,
    }),

    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewType, chatProvider, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    (() => {
      const deps = edition8Deps(context);
      return registerEdition8(context, {
        deps,
        view: { show: (screen: string) => chatProvider?.showEdition8Screen(screen) },
        openSettings: (screen: string) => chatProvider?.showSettings(screen, screen === 'S29'),
        cancelRun: () => cancelActiveRun(),
        switchConversation: () => switchSessionFromCommand(context),
      });
    })(),
    vscode.commands.registerCommand('faktor.startServer', async () => {
      try {
        await startServer(context);
      } catch (error) {
        reportError(error, 'daemon_start_failed');
      }
    }),
    vscode.commands.registerCommand('faktor.stopServer', () => {
      stopServer(context);
    }),
    // T02/T04 `Locate daemon…` and the palette entry: ONE machine-scoped
    // open-file write path (never a workspace value, never an execution).
    vscode.commands.registerCommand('faktor.locateDaemon', async () => {
      await locateDaemon(context);
    }),
    vscode.commands.registerCommand('faktor.openChat', () => {
      chatProvider?.focus();
    }),
    vscode.commands.registerCommand('faktor.openControlCenter', () => {
      openControlCenter(context, 'S29');
    }),
    vscode.commands.registerCommand('faktor.openModelPicker', () => {
      void pickModel(edition8Deps(context));
    }),
    vscode.commands.registerCommand('faktor.openSettings', () => {
      openControlCenter(context, 'S29');
    }),
    vscode.commands.registerCommand('faktor.newTask', async () => {
      await newTaskFromCommand(context);
    }),
    vscode.commands.registerCommand('faktor.switchSession', async () => {
      await switchSessionFromCommand(context);
    }),
    vscode.commands.registerCommand('faktor.cancelTask', () => cancelActiveRun()),
    vscode.commands.registerCommand('faktor.replyPermission', () => replyPermissionFromCommand()),
    vscode.commands.registerCommand('faktor.refresh', () => refreshFromUserGesture()),
    // The palette entry for the same recovery the webview gesture triggers:
    // a blocked stream is recoverable without finding the panel button.
    vscode.commands.registerCommand('faktor.reconnectStream', () => recoverEventStream()),
    vscode.commands.registerCommand('faktor.controlPlaneSignIn', async () => {
      await controlPlaneSignIn(context);
    }),
    vscode.commands.registerCommand('faktor.controlPlaneSignOut', async () => {
      await controlPlaneSignOut(context);
    }),
    {
      dispose: store.subscribe((snapshot) => {
        chatProvider?.postSnapshot(snapshot);
        updateStatusBar();
      }),
    },
    { dispose: () => secretWatch.dispose() },
    // A machine-scoped daemon setting changed (e.g. through Settings or
    // `Locate daemon…`): re-derive the ONE preflight view so the card and the
    // Diagnostics mirror always name the exact effective executable.
    vscode.workspace.onDidChangeConfiguration((event) => {
      if (
        event.affectsConfiguration('faktor.binaryPath') ||
        event.affectsConfiguration('faktor.installRoot') ||
        event.affectsConfiguration('faktor.dataDir')
      ) {
        const authority = store.snapshot().serviceAuthority;
        if (
          authority !== null &&
          authority.kind !== 'startup_budget_exhausted' &&
          authority.kind !== 'workspace_untrusted' &&
          authority.kind !== 'workspace_unavailable'
        ) {
          // Fixing the machine-scoped configuration is an explicit operator
          // action: retry the same strict gate once, without bypassing the
          // restart budget.
          void serviceLifecycle?.startExplicit();
        } else {
          publishPreflight(context);
        }
      }
    }),
    // Granting trust is an explicit operator decision: the paused automatic
    // start may now proceed without asking the user to find a Start button.
    vscode.workspace.onDidGrantWorkspaceTrust(() => {
      if (store.snapshot().serviceAuthority?.kind === 'workspace_untrusted') {
        void serviceLifecycle?.startExplicit();
      }
    }),
    { dispose: () => stopServer() },
  );

  // Opening the Faktor view (or any Faktor command that activates the
  // extension) auto-starts the verified installed service. The supervisor
  // waits for the short connecting threshold before showing anything, and an
  // explicit Diagnostics stop keeps auto-start off until Start is pressed.
  void ensureServiceLifecycle(context).open();
  // Item 17: the provenance surface is available even before the daemon
  // connects (extension assets + the packaged stamp; daemon rows fill in).
  refreshProvenance(context);
}

export async function deactivate(): Promise<void> {
  await serviceLifecycle?.dispose();
}
