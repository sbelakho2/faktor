// The Faktor VS Code product surface over the native daemon:
//
//   - daemon.ts        spawns and owns the faktor-cli process
//   - nativeClient.ts  strict typed fetch client of /native/* (+ the
//                      minimal /session/create|/session/list surface)
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
import { resolve } from 'node:path';
import { randomUUID } from 'node:crypto';
import { DaemonHandle, startDaemon, stopDaemon } from './daemon';
import {
  FetchLike,
  NativeApiError,
  NativeBillingUsage,
  NativeClient,
  NativeCompletionContract,
  NativeEntitlementSnapshot,
  NativeEvidenceSelector,
  NativeIdentity,
  NativeMessagePage,
  NativeModelInfo,
  NativePermissionEntry,
  NativeSessionUsage,
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
  SseFetchLike,
  SseResponseLike,
} from './eventStream';
import {
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
  applySseEvent,
  boardStateFromPage,
  cancelRunTarget,
  nextPixelPresence,
  parseBoardPostRequest,
  parseBoardReadRequest,
  summarizeAgents,
  transcriptFromMessages,
  unavailableBoardState,
} from './state';
import { CockpitTaskVerification, CockpitUsagePanel, buildCockpit, buildUsagePanel, cockpitSections, tournamentViewOf, usagePanelSections } from './cockpit';
import type { PixelPresence } from './pixelAgents';
import {
  microBalance,
  microFromNumber,
  microText,
  microToString,
  MICRO_I64_MAX,
  sumMicro,
  type MicroMoney,
} from './money.ts';
import {
  AdmitFailure,
  PendingSubmission,
  StartFailure,
  StartTaskSettings,
  admitPendingSubmission,
  boundedWebviewFiles,
  hasCompletionSteps,
  parseCompletionContract,
} from './taskStart';
import {
  SessionBindings,
  boundSessionFor,
  canonicalWorkspaceKey,
  pruneBindings,
  withBinding,
} from './workspaceBinding';
import { MAX_STEER_CHARS, normalizeSteerText } from './steer';
import {
  ControlPlaneAuthSession,
  ControlPlaneResolution,
  ControlPlaneScope,
  controlPlaneAuthSessionValid,
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
/** Default cloud control-plane coordinates; both are non-secret. */
const DEFAULT_CONTROL_PLANE_ENDPOINT = 'https://api.faktor.dev';
/** The last control-plane notice, so a repeated refusal never spams the chat. */
let lastControlPlaneNotice: string | null = null;

interface ActiveSession {
  daemon: DaemonHandle | null;
  client: NativeClient | null;
  stream: EventStream | null;
  sessionId: string | null;
  activeRunId: string | null;
  refreshing: boolean;
  refreshTimer: NodeJS.Timeout | null;
  /** Reuse the last task-verification read while its inputs are unchanged. */
  taskVerificationKey: string | null;
  taskVerification: NativeTaskVerification | null;
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
  billingCursor: string | null;
  billingPrevCursors: string[];
}

const active: ActiveSession = {
  daemon: null,
  client: null,
  stream: null,
  sessionId: null,
  activeRunId: null,
  refreshing: false,
  refreshTimer: null,
  taskVerificationKey: null,
  taskVerification: null,
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
  billingCursor: null,
  billingPrevCursors: [],
};

const store = new FaktorStore();
let chatProvider: ChatViewProvider | null = null;
let statusBar: vscode.StatusBarItem | null = null;

// ------------------------------------------------------------------ helpers

function config<T>(key: string, fallback: T): T {
  return vscode.workspace.getConfiguration('faktor').get<T>(key, fallback);
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
 * The exact canonical workspace identity of THIS window: the first folder
 * URI string (or the extension root for a folderless window). Never an
 * index into a session list, never a filesystem case-fold.
 */
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

function reportError(error: unknown): void {
  const message = messageOf(error);
  if (store.snapshot().daemon === 'starting') {
    store.patch({ daemon: 'error', daemonDetail: message });
  }
  store.patch({ lastError: message });
  chatProvider?.postNotice('error', message);
  void vscode.window.showErrorMessage(`Faktor: ${message}`);
}

// ------------------------------------------------- control-plane credentials
//
// The cloud control-plane credential lives in `vscode.SecretStorage` keyed by
// the (non-secret) endpoint + organization coordinates. The deprecated
// plaintext `faktor.controlToken` setting is read once for a migration prompt
// and then deleted; its value is NEVER handed to the native client (the
// daemon-password `bearerToken` path is separate and unchanged).

function controlPlaneNotice(level: 'info' | 'error', text: string): void {
  if (lastControlPlaneNotice === text) {
    return;
  }
  lastControlPlaneNotice = text;
  chatProvider?.postNotice(level, text);
}

function controlPlaneScope(): ControlPlaneScope | null {
  const scope: ControlPlaneScope = {
    endpoint: config('controlPlaneEndpoint', ''),
    organization: config('controlPlaneOrganization', ''),
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
    sessionId: config('controlPlaneAuthSession', ''),
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
      const raw = config('controlToken', '');
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
    prompt: 'Cloud control-plane endpoint the credential belongs to',
    value: config('controlPlaneEndpoint', '') || DEFAULT_CONTROL_PLANE_ENDPOINT,
    ignoreFocusOut: true,
  });
  if (endpoint === undefined || endpoint.trim().length === 0) {
    return null;
  }
  const organization = await vscode.window.showInputBox({
    title: 'Faktor: migrate control-plane credential',
    prompt: 'Organization (or account) id the credential belongs to',
    value: config('controlPlaneOrganization', ''),
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
    prompt: 'Cloud control-plane endpoint (non-secret; keys the stored credential)',
    value: current?.endpoint ?? DEFAULT_CONTROL_PLANE_ENDPOINT,
    ignoreFocusOut: true,
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
    value: config('controlPlaneAuthSession', ''),
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

async function startServer(context: vscode.ExtensionContext): Promise<void> {
  if (active.daemon && (await active.daemon.alive()) && active.client) {
    if (!active.sessionId) {
      await ensureSession(active.client, context);
      startStream();
    }
    return;
  }
  store.patch({ daemon: 'starting', daemonDetail: 'locating faktor-cli', lastError: null });
  const binaryPath = config('binaryPath', '');
  const dataDir = config('dataDir', '');
  const installRoot = config('installRoot', '');
  const extraArgs = config<string[]>('extraArgs', []);
  // The default exceeds the bootstrap launcher's ~30 s launch window: the
  // supervisor must not declare failure while the updater launcher may still
  // be bringing the release to readiness (and may still be adoptable).
  const startupTimeoutMs = config('startupTimeoutMs', 45_000);
  const daemon = await startDaemon({
    workspaceRoot: workspaceRoot(context),
    binaryPath: binaryPath.length > 0 ? binaryPath : undefined,
    dataDir: dataDir.length > 0 ? dataDir : undefined,
    // When an immutable install layout exists at this root, the daemon is
    // spawned through its stable bootstrap launcher so the activated release
    // (and only that release) runs. No layout => legacy resolution.
    installRoot: installRoot.length > 0 ? installRoot : undefined,
    extraArgs,
    startupTimeoutMs,
  });
  try {
    const client = new NativeClient({
      baseUrl: daemon.baseUrl,
      bearerToken: daemon.bearerToken,
      fetch: fetchAdapter(),
    });
    // The control-plane credential is resolved from the OS/IDE secret store
    // (never from the deprecated plaintext setting) before the first read.
    await applyControlPlaneCredential(client, context);
    const health = await client.health();
    active.daemon = daemon;
    active.client = client;
    store.patch({
      daemon: 'running',
      // The health version already carries the bootstrap-verified release
      // digest when the daemon was launched through an install layout.
      daemonDetail: `${health.version} on port ${daemon.port}`,
      baseUrl: daemon.baseUrl,
      lastError: null,
    });
    await ensureSession(client, context);
    startStream();
    scheduleRefresh(0);
    chatProvider?.postNotice('info', `daemon ${health.version} ready at ${daemon.baseUrl}`);
  } catch (error) {
    // Never leak a spawned daemon when post-spawn setup fails.
    stopDaemon(daemon);
    active.daemon = null;
    active.client = null;
    store.patch({ daemon: 'error', daemonDetail: messageOf(error), baseUrl: null });
    throw error;
  }
}

function stopServer(): void {
  if (active.refreshTimer !== null) {
    clearTimeout(active.refreshTimer);
    active.refreshTimer = null;
  }
  active.stream?.stop();
  active.stream = null;
  stopDaemon(active.daemon);
  active.daemon = null;
  active.client = null;
  active.sessionId = null;
  active.activeRunId = null;
  active.refreshing = false;
  active.taskVerificationKey = null;
  active.taskVerification = null;
  active.taskProofKey = null;
  active.taskProof = null;
  active.taskProofUnavailable = null;
  active.tournamentId = null;
  active.pixelPresence = new Map();
  active.completionContract = null;
  active.boardSeenRevision = 0;
  active.billingIdentity = null;
  active.billingEntitlements = null;
  active.billingUsage = null;
  active.billingCursor = null;
  active.billingPrevCursors = [];
  store.patch({
    daemon: 'stopped',
    daemonDetail: '',
    baseUrl: null,
    session: null,
    machineState: 'unknown',
    machineLabel: 'Daemon stopped',
    sessions: [],
    runs: [],
    activeRunId: null,
    agents: [],
    task: null,
    verification: null,
    usage: null,
    cockpit: null,
    cockpitSections: [],
    tournament: null,
    transcript: [],
    streamStatus: 'stopped',
    lastError: null,
    busy: false,
  });
}

async function ensureSession(
  client: NativeClient,
  context: vscode.ExtensionContext,
): Promise<string> {
  if (active.sessionId) {
    return active.sessionId;
  }
  const previousSessionId = active.sessionId;
  const workspace = canonicalWorkspace(context);
  let sessions: Awaited<ReturnType<NativeClient['listSessions']>> = [];
  let listed = false;
  try {
    sessions = await client.listSessions();
    listed = true;
    store.patch({ sessions });
  } catch (error) {
    store.patch({ lastError: messageOf(error) });
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
    store.patch({ session: boundSummary });
  } else if (boundId === null && !listed && workspace.key !== null && bindings[workspace.key]) {
    // The listing failed; trust the durable binding for this exact
    // workspace rather than minting a duplicate session.
    active.sessionId = bindings[workspace.key] as string;
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
    const created = await client.createSession({
      provider: provider || 'faktor',
      model: model || 'default',
      workspace: workspace.fsPath,
      title: workspaceTitle(),
    });
    active.sessionId = created.id;
    bindings = withBinding(bindings, workspace.key, created.id);
    await writeBindings(context, bindings);
    store.patch({
      session: {
        id: created.id,
        title: created.title,
        provider: provider || '',
        model: model || '',
        state: 'unknown',
      },
    });
  }
  const sessionId = active.sessionId;
  if (sessionId !== previousSessionId) {
    // The board watermark belongs to ONE session/run family.
    active.boardSeenRevision = 0;
  }
  try {
    const page = await client.messages(sessionId, { limit: HISTORY_PAGE_LIMIT });
    store.patch({ transcript: transcriptOf(page) });
  } catch (error) {
    store.patch({ lastError: messageOf(error) });
  }
  return sessionId;
}

/** Cheap structural signature: refresh the transcript only when it changed. */
function transcriptSignature(entries: readonly TranscriptEntry[]): string {
  let out = '';
  for (const entry of entries) {
    out += `${entry.id}:${entry.text.length}:${entry.reasoning.length}:${entry.summary.length}:`;
    for (const tool of entry.tools) {
      out += `${tool.state}:${tool.excerpt?.length ?? 0}:${tool.exitCode ?? 'n'};`;
    }
    out += '|';
  }
  return out;
}

function transcriptOf(page: NativeMessagePage): TranscriptEntry[] {
  return transcriptFromMessages(page.messages as unknown as Json[]);
}

function startStream(): void {
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
    fetch: sseAdapter(),
    onEvent: (frame) => {
      const snapshot = store.snapshot();
      const transcript = applySseEvent(snapshot.transcript, frame.event, frame.data);
      if (transcript !== snapshot.transcript) {
        store.patch({ transcript });
      }
      scheduleRefresh();
    },
    onStatus: (status: EventStreamStatus) => {
      store.patch({ streamStatus: status });
      updateStatusBar();
    },
    onError: (error) => {
      store.patch({ lastError: messageOf(error) });
    },
  });
  active.stream = stream;
  stream.start();
  store.patch({ streamStatus: 'connecting' });
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

async function refresh(): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  if (!client || !sessionId || active.refreshing) {
    return;
  }
  active.refreshing = true;
  try {
    // The board read is OPTIONAL and never rejects: an older daemon records
    // an explicit unavailable block instead of blanking the snapshot.
    const boardPromise = boardFor(client, sessionId);
    const [projection, tasks, verification, usage, agents, runs, sessions, messages, catalog] =
      await Promise.all([
        client.projection(sessionId),
        client.tasks(sessionId),
        client.verification(sessionId),
        client.sessionUsage(sessionId),
        client.agents(sessionId),
        client.taskRuns(sessionId),
        client.listSessions(),
        client.messages(sessionId, { limit: HISTORY_PAGE_LIMIT }),
        // The catalog only enriches agent metadata; a catalog failure must
        // not blank the rest of the snapshot.
        client.modelCatalog().catch(() => [] as NativeModelInfo[]),
      ]);
    const board = await boardPromise;
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
    const taskVerification = cockpitTaskVerificationView(
      await taskVerificationFor(client, sessionId, runs, task, verification),
    );
    // The strict proof summary feeds the top-level VERIFIED view: a failed
    // read is an EXPLICIT unavailable state (the cached proof is dropped),
    // so a stale VERIFIED can never render for an unreadable proof.
    const proofRead = await taskProofFor(client, sessionId, runs, task);
    // The durable tournament auto-load: the tracked id while it still exists,
    // else the newest summary. A missing listing/state is a null block, never
    // a fabricated tournament.
    const tournament = tournamentViewOf(await tournamentFor(client, sessionId));
    // The commercial-metering panel is best-effort like the board: a refusal
    // becomes an explicit disabled/unavailable panel, never a snapshot error.
    const usagePanel = await billingPanelFor(client);
    const cockpit = buildCockpit({
      task,
      agents: agentSummaries,
      verification: verificationView,
      usage: usageView,
      taskVerification,
      tournament,
      proof: proofRead.proof,
      proofUnavailable: proofRead.unavailable,
    });
    const sections = cockpit === null ? [] : cockpitSections(cockpit);
    store.patch({
      sessions,
      machineState: projection.state.machine,
      machineLabel: projection.state.label,
      task,
      verification: verificationView,
      usage: usageView,
      agents: agentSummaries,
      runs: runs.map(runSummary),
      activeRunId,
      busy: activeRunId !== null,
      cockpit,
      cockpitSections: [...sections, ...usagePanelSections(usagePanel)],
      usagePanel,
      tournament: cockpit?.tournament ?? null,
      board,
      lastError: null,
    });
    // Assistant/status/tool lines are durable message rows; re-render the
    // bounded newest page only when its structure actually changed.
    const transcript = transcriptOf(messages);
    if (transcriptSignature(transcript) !== transcriptSignature(store.snapshot().transcript)) {
      store.patch({ transcript });
    }
  } catch (error) {
    store.patch({ lastError: messageOf(error) });
  } finally {
    active.refreshing = false;
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
 * reusing the cached read while its inputs are unchanged. Optional: a
 * failure degrades to the previous read (or null), never an error patch.
 */
async function taskVerificationFor(
  client: NativeClient,
  sessionId: string,
  runs: readonly NativeTaskRun[],
  task: TaskSummary | null,
  verification: NativeVerificationView,
): Promise<NativeTaskVerification | null> {
  if (task === null || runs.length === 0) {
    active.taskVerificationKey = null;
    active.taskVerification = null;
    return null;
  }
  const taskId = String(runs[0]!.task_id);
  const key = `${taskId}:${task.state}:${verification.failedChecks.length}:${verification.owed.length}`;
  if (key === active.taskVerificationKey) {
    return active.taskVerification;
  }
  try {
    const view = await client.taskVerification(sessionId, taskId);
    active.taskVerificationKey = key;
    active.taskVerification = view;
    return view;
  } catch {
    return active.taskVerification;
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
    const [entitlementsView, usage] = await Promise.all([
      client.entitlements(),
      client.billingUsage(identityView.identity.organization, {
        since: cursor,
        limit: BILLING_PAGE_LIMIT,
      }),
    ]);
    active.billingEntitlements = entitlementsView.entitlements;
    active.billingUsage = usage;
    return buildUsagePanel({
      identity: identityView.identity,
      entitlements: entitlementsView.entitlements,
      usage,
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
    return buildUsagePanel({
      identity: active.billingIdentity,
      entitlements: null,
      usage: null,
      refusal,
      cursor,
      hasPrev,
    });
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
  const parsed = parseBoardPostRequest({
    subject: message.subject,
    body: message.body,
    refs: message.refs,
  });
  if ('reason' in parsed) {
    chatProvider?.postNotice('error', parsed.reason);
    return;
  }
  if (!client || !sessionId) {
    chatProvider?.postNotice('info', 'start the daemon before posting to the board');
    return;
  }
  try {
    const post = await client.boardPost(sessionId, parsed);
    // The operator authored this post: it is read by definition.
    active.boardSeenRevision = Math.max(active.boardSeenRevision, post.revision);
    chatProvider?.postNotice('info', `board post #${post.revision} recorded`);
    scheduleRefresh(0);
  } catch (error) {
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

async function startTask(
  goal: string,
  files: readonly string[],
  contract: NativeCompletionContract | null,
  context: vscode.ExtensionContext,
  pending: PendingSubmission,
): Promise<void> {
  const restore = (failure: AdmitFailure): void => {
    reportError(new Error(failure.message));
    // NEVER clear before acceptance: the restore callback carries the
    // original text back to the composer and rebuilds the draft.
    chatProvider?.postSendMessageFailed(pending, failure.message);
  };
  try {
    if (!active.client || !active.sessionId) {
      await startServer(context);
    }
    const client = active.client;
    const sessionId = active.sessionId;
    if (!client || !sessionId) {
      restore({
        kind: 'transport',
        stage: 'start',
        status: null,
        code: null,
        message: 'daemon/session unavailable; the draft and attachments were kept',
      });
      return;
    }
    const budgetCostRaw = config('budgetCostMicro', 0);
    const maxCostMicro = microFromNumber(budgetCostRaw);
    if (maxCostMicro === null) {
      // A configured micro amount above 2^53-1 cannot be represented exactly
      // by a VS Code number setting; refusing is the only honest option (the
      // run must never start with a silently rounded budget).
      reportError(
        new Error(
          `faktor.budgetCostMicro ${budgetCostRaw} is not an exact non-negative integer micro amount (max 2^53-1); the task was not started`,
        ),
      );
      return;
    }
    const settings: StartTaskSettings = {
      // Shadow-only: empty (the default) inherits the daemon's sole mode and
      // "shadow" names it explicitly. The removed direct_compat value is a
      // typed refusal at admission; a refused shadow run never downgrades.
      mutationMode: config('mutationMode', ''),
      maxTokens: config('budgetTokens', 0),
      maxCostMicro,
      files,
      completionContract: contract,
    };
    const outcome = await admitPendingSubmission({
      client,
      sessionId,
      pending,
      settings,
      onStarted: (started) => {
        active.activeRunId = started.run_id;
        active.completionContract = contract;
        store.patch({ activeRunId: started.run_id, busy: true, lastError: null });
        const attached = files.length + pending.attachments.length;
        const attachments = attached > 0 ? ` with ${attached} attachment(s)` : '';
        const steps = contract !== null ? ' + completion contract' : '';
        chatProvider?.postNotice(
          'info',
          `task run ${started.run_id} started (${started.state})${attachments}${steps}`,
        );
        scheduleRefresh(0);
      },
      onFailure: (failure: StartFailure | AdmitFailure) => {
        reportError(new Error(failure.message));
      },
      restore,
    });
    if (outcome.ok) {
      // Durable acceptance: ONLY now may the pending envelope be dropped.
      chatProvider?.postStartResult(goal, true);
    }
  } catch (error) {
    reportError(error);
    chatProvider?.postSendMessageFailed(
      pending,
      error instanceof Error ? error.message : String(error),
    );
  }
}

/**
 * Task-mode completion controls for the command path (the chat composer
 * carries the same three checkboxes): a multi-select list of the three
 * conditional steps. An empty selection = today's default path (no
 * contract, no work item).
 */
async function promptCompletionContract(): Promise<NativeCompletionContract | null> {
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
  if (picks === undefined || picks.length === 0) {
    return null;
  }
  return {
    include_commit: picks.some((pick) => pick.key === 'include_commit'),
    include_push: picks.some((pick) => pick.key === 'include_push'),
    include_pr: picks.some((pick) => pick.key === 'include_pr'),
  };
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
  await startTask(goal.trim(), [], contract, context, pendingEnvelope(goal.trim()));
}

async function cancelActiveRun(): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  // Target ONLY an active (non-terminal) run: a terminal run in the list is
  // not cancellable and the server's typed 409 is never provoked.
  const runId = cancelRunTarget(active.activeRunId, store.snapshot().runs);
  if (!client || !sessionId || runId === null) {
    chatProvider?.postNotice('info', 'no active task run to cancel');
    return;
  }
  try {
    const ack = await client.cancelTaskRun(sessionId, runId);
    chatProvider?.postNotice('info', `run ${ack.run_id} cancel requested`);
    active.activeRunId = null;
    store.patch({ activeRunId: null, busy: false });
    scheduleRefresh(0);
  } catch (error) {
    reportError(error);
  }
}

/**
 * Reply to ONE live pending permission of the active session
 * (`faktor.replyPermission`). The picker lists exactly the daemon's pending
 * set, and the strict reply body carries the OWNING session id from the
 * selected entry — never a guessed session. Typed 409 refusals surface as an
 * explicit notice/UI state (`unknown_or_resolved` vs `session_mismatch`) and
 * are NEVER retried blindly; the pending list is refreshed either way so the
 * operator works from the daemon's current truth.
 */
async function replyPermissionFromCommand(): Promise<void> {
  const client = active.client;
  const sessionId = active.sessionId;
  if (!client || !sessionId) {
    chatProvider?.postNotice('info', 'start the daemon and open a session first');
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
  try {
    await client.replyPermission(picked.permission.sessionId, picked.permission.id, decision.value);
    chatProvider?.postNotice('info', `permission #${picked.permission.id}: ${decision.value}`);
    scheduleRefresh(0);
  } catch (error) {
    const failure = classifyPermissionReplyFailure(error);
    if (failure === null) {
      reportError(error);
      return;
    }
    const text = permissionReplyFailureMessage(failure, picked.permission.id);
    chatProvider?.postNotice('error', text);
    void vscode.window.showErrorMessage(`Faktor: ${text}`);
    scheduleRefresh(0);
  }
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
    await refresh();
    return;
  }
  if (action === 'usage-prev') {
    if (active.billingPrevCursors.length === 0) {
      chatProvider?.postNotice('info', 'already at the first usage page');
      return;
    }
    const previous = active.billingPrevCursors.pop() ?? '';
    active.billingCursor = previous.length > 0 ? previous : null;
    await refresh();
    return;
  }
  if (action === 'grant-credits') {
    await grantCreditsFromCommand();
    return;
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
    prompt: 'Credit grant amount in microUSD (integer, > 0, exact)',
    placeHolder: '1000000',
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
  if (!/^[0-9]+$/.test(text)) {
    return { ok: false, reason: 'enter a positive integer amount in microUSD' };
  }
  const value = BigInt(text);
  if (value <= 0n) {
    return { ok: false, reason: 'the grant amount must be greater than zero' };
  }
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

// --------------------------------------------------------------- status bar

function updateStatusBar(): void {
  if (!statusBar) {
    return;
  }
  const snapshot = store.snapshot();
  const icons: Record<string, string> = {
    running: '$(pulse)',
    starting: '$(sync~spin)',
    error: '$(error)',
    stopped: '$(circle-slash)',
  };
  const icon = icons[snapshot.daemon] ?? '$(circle-slash)';
  const task = snapshot.task ? ` · task ${snapshot.task.state}` : '';
  statusBar.text = `${icon} Faktor: ${snapshot.daemon}${task}`;
  statusBar.tooltip = snapshot.baseUrl
    ? `Faktor daemon ${snapshot.daemon} at ${snapshot.baseUrl}${snapshot.session ? ` · session ${snapshot.session.title}` : ''}`
    : 'Faktor daemon stopped';
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
    case 'startDaemon':
      try {
        await startServer(context);
      } catch (error) {
        reportError(error);
      }
      return;
    case 'stopDaemon':
      stopServer();
      return;
    case 'refresh':
      await refresh();
      return;
    case 'sendGoal': {
      const goal = typeof message.goal === 'string' ? message.goal.trim() : '';
      if (goal.length === 0) {
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
        const reasons = refused
          .slice(0, 5)
          .map((entry) => `#${entry.index}: ${entry.reason}`)
          .join('; ');
        chatProvider?.postNotice(
          'error',
          `${refused.length} attachment(s) refused: ${reasons}`,
        );
      }
      const contract = parseCompletionContract(message.completionContract);
      if ('reason' in contract) {
        chatProvider?.postNotice('error', `task start refused: ${contract.reason}`);
        chatProvider?.postStartResult(goal, false);
        return;
      }
      // The composer carries no binary refs: only workspace-relative file
      // paths reach the native run. Unknown binary-shaped members are noted
      // (never forwarded, never silently dropped).
      const binaryRefs = Array.isArray(message.attachments) ? message.attachments.length : 0;
      if (binaryRefs > 0) {
        chatProvider?.postNotice(
          'info',
          `${binaryRefs} binary attachment reference(s) noted; only workspace-relative file paths reach the native run`,
        );
      }
      await startTask(goal, files, contract.contract, context, pendingEnvelope(goal));
      return;
    }
    case 'newTask':
      await newTaskFromCommand(context);
      return;
    case 'cancelRun':
      await cancelActiveRun();
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
    case 'retrieveEvidence':
      await retrieveEvidence(message);
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

// ------------------------------------------------------------------- lifecycle

export function activate(context: vscode.ExtensionContext): void {
  statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 100);
  statusBar.command = 'faktor.openChat';
  context.subscriptions.push(statusBar);
  updateStatusBar();

  chatProvider = new ChatViewProvider(context.extensionUri, {
    handle: (message) => handleWebviewMessage(message, context),
  });

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
    vscode.window.registerWebviewViewProvider(ChatViewProvider.viewType, chatProvider, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
    vscode.commands.registerCommand('faktor.startServer', async () => {
      try {
        await startServer(context);
      } catch (error) {
        reportError(error);
      }
    }),
    vscode.commands.registerCommand('faktor.stopServer', () => {
      stopServer();
    }),
    vscode.commands.registerCommand('faktor.openChat', () => {
      chatProvider?.focus();
    }),
    vscode.commands.registerCommand('faktor.newTask', async () => {
      await newTaskFromCommand(context);
    }),
    vscode.commands.registerCommand('faktor.cancelTask', () => cancelActiveRun()),
    vscode.commands.registerCommand('faktor.replyPermission', () => replyPermissionFromCommand()),
    vscode.commands.registerCommand('faktor.refresh', () => refresh()),
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
    { dispose: () => stopServer() },
  );

  if (config('autoStart', false)) {
    void startServer(context).catch(reportError);
  }
}

export function deactivate(): void {
  stopServer();
}
