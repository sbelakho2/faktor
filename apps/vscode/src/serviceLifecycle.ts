// The transparent, bounded service lifecycle.
//
// Normal users never operate a daemon: opening Faktor resolves the
// machine-scoped, trust-verified executable and starts it automatically.
// Only genuine authority (missing binary, changed identity/checksum, an
// exhausted restart budget, invalid configuration or a required operator
// decision) interrupts the user.
//
// Invariants encoded here:
//  - Start is SINGLE-FLIGHT: concurrent open/explicit/restart triggers share
//    one launch, so two daemon processes can never be spawned.
//  - Every restart re-runs `prepare()` before `launch()`: a changed checksum,
//    a vanished binary or invalid configuration can never be started.
//  - Restarts are bounded by a rolling budget with exponential backoff; an
//    exhausted budget is a typed authority state, never an infinite loop.
//  - An explicit stop disables auto-start/restart until an explicit start.
//  - Session resume runs at most once per successful connection generation.
//  - The healthy state publishes no service notice: normal UI stays quiet.
//
// Dependency-free (no vscode import) so scripts/selftest.mjs can drive the
// full lifecycle with fake executables and connections.

import { isAbsolute, relative, resolve } from 'node:path';
import type { BinaryTrust, DaemonResolutionSource } from './daemon';

export type ServiceMode = 'auto' | 'restart' | 'explicit';

export interface ServiceLaunchPlan {
  readonly executable: string;
  readonly source: DaemonResolutionSource;
  readonly trust: BinaryTrust;
  readonly installRoot: string;
  readonly dataDir?: string;
  readonly extraArgs: readonly string[];
  readonly startupTimeoutMs: number;
}

/** The minimum a supervised connection must expose. */
export interface ServiceConnection {
  readonly port: number;
  readonly pid?: number;
  readonly baseUrl: string;
  readonly version: string;
  alive(): Promise<boolean>;
}

export type ServicePhase =
  | 'idle'
  | 'connecting'
  | 'running'
  | 'reconnecting'
  | 'stopped_by_user'
  | 'blocked';

export type ConnectionNotice = 'connecting' | 'reconnecting' | null;

export type ServiceAuthorityKind =
  | 'binary_missing'
  | 'trust_changed'
  | 'trust_unverified'
  | 'outside_trusted_install'
  | 'startup_budget_exhausted'
  | 'configuration_invalid'
  | 'workspace_untrusted'
  | 'workspace_unavailable'
  | 'user_action_required';

export type ServiceAuthorityActionKey =
  | 'locateDaemon'
  | 'openDiagnostics'
  | 'retryStart'
  | 'reviewTrust'
  | 'manageWorkspaceTrust'
  | 'openProject';

export interface ServiceAuthorityAction {
  readonly key: ServiceAuthorityActionKey;
  readonly label: string;
}

export interface ServiceAuthority {
  readonly kind: ServiceAuthorityKind;
  readonly label: string;
  readonly copy: string;
  readonly actions: readonly ServiceAuthorityAction[];
  /** Exact bounded technical detail (Diagnostics / disclosure only). */
  readonly detail: string;
}

export interface ServiceDiagnosticsView {
  readonly phase: ServicePhase;
  readonly executable: string | null;
  readonly source: DaemonResolutionSource | null;
  readonly trust: BinaryTrust['state'] | null;
  /** The launch resolves only after GET /native/ready, so running = ready. */
  readonly readiness: 'idle' | 'waiting' | 'ready' | 'failed';
  readonly readinessDetail: string | null;
  readonly port: number | null;
  readonly pid: number | null;
  readonly baseUrl: string | null;
  readonly restarts: number;
  readonly restartBudget: number;
  readonly lastFailure: string | null;
}

export interface ServiceLifecycleView {
  readonly phase: ServicePhase;
  /** Transient normal-UI notice only; null while healthy. */
  readonly connectionNotice: ConnectionNotice;
  /** Genuine authority; null while healthy or merely reconnecting. */
  readonly authority: ServiceAuthority | null;
  readonly diagnostics: ServiceDiagnosticsView;
}

export interface ServicePreparationFailure {
  readonly kind: ServiceAuthorityKind;
  readonly detail: string;
  /** A transient failure is retried under the budget; authority is not. */
  readonly retryable?: boolean;
}

export type ServicePreparation =
  | { readonly ok: true; readonly plan: ServiceLaunchPlan }
  | { readonly ok: false; readonly failure: ServicePreparationFailure };

export interface ServiceStartRequest {
  /** Diagnostics explicit start may confirm an unverified path. */
  readonly allowUnverified: boolean;
}

export interface ServiceLifecycleConfig {
  /** How long a normal start may take before "Connecting to Faktor…" shows. */
  readonly connectingNoticeMs: number;
  readonly probeIntervalMs: number;
  readonly backoffBaseMs: number;
  readonly backoffMaxMs: number;
  readonly maxRestarts: number;
  readonly restartWindowMs: number;
  /** A connection that stays healthy this long refills the restart budget. */
  readonly stableResetMs: number;
  /** 0..1; tests inject () => 0 for deterministic backoff. */
  readonly jitter: () => number;
}

export const DEFAULT_SERVICE_CONFIG: ServiceLifecycleConfig = {
  connectingNoticeMs: 400,
  probeIntervalMs: 2_000,
  backoffBaseMs: 500,
  backoffMaxMs: 15_000,
  maxRestarts: 5,
  restartWindowMs: 5 * 60_000,
  stableResetMs: 60_000,
  jitter: () => Math.random(),
};

export interface ServiceLifecycleDeps<C extends ServiceConnection> {
  prepare(
    mode: ServiceMode,
    request: ServiceStartRequest,
  ): Promise<ServicePreparation> | ServicePreparation;
  launch(plan: ServiceLaunchPlan, signal: AbortSignal): Promise<C>;
  probe(connection: C): Promise<boolean>;
  shutdown(connection: C, reason: 'crash' | 'stop' | 'dispose'): Promise<void>;
  /** Reconcile/rebind/resume existing sessions; called once per generation. */
  resume(connection: C, generation: number): Promise<void>;
  publish(view: ServiceLifecycleView): void;
  /** Only genuine authority reaches this callback. */
  onAuthority?(authority: ServiceAuthority): void;
  readonly now?: () => number;
  readonly setTimeout?: (callback: () => void, ms: number) => unknown;
  readonly clearTimeout?: (handle: unknown) => void;
  readonly config?: Partial<ServiceLifecycleConfig>;
}

/**
 * The ONE authority copy table. Each row has a concrete next step; a
 * reconnecting/healthy service never produces a row.
 */
export function serviceAuthority(kind: ServiceAuthorityKind, detail: string): ServiceAuthority {
  switch (kind) {
    case 'binary_missing':
      return {
        kind,
        label: 'Faktor needs its service',
        copy: 'Faktor could not find the installed service on this machine. Locate the installed executable to continue.',
        actions: [
          { key: 'locateDaemon', label: 'Locate service…' },
          { key: 'openDiagnostics', label: 'Open Diagnostics' },
        ],
        detail,
      };
    case 'trust_changed':
      return {
        kind,
        label: 'Faktor stopped for safety',
        copy: 'The service executable changed or no longer matches its recorded identity. Faktor did not start it.',
        actions: [
          { key: 'reviewTrust', label: 'Review executable' },
          { key: 'openDiagnostics', label: 'Open Diagnostics' },
        ],
        detail,
      };
    case 'trust_unverified':
      return {
        kind,
        label: 'Faktor needs a trust decision',
        copy: 'The installed service has no recorded identity. Review it in Diagnostics before Faktor can start it.',
        actions: [{ key: 'openDiagnostics', label: 'Review in Diagnostics' }],
        detail,
      };
    case 'outside_trusted_install':
      return {
        kind,
        label: 'Faktor refused an untrusted executable',
        copy: 'The configured service executable is outside the trusted install, so Faktor did not start it.',
        actions: [
          { key: 'locateDaemon', label: 'Locate service…' },
          { key: 'openDiagnostics', label: 'Open Diagnostics' },
        ],
        detail,
      };
    case 'startup_budget_exhausted':
      return {
        kind,
        label: 'Faktor could not stay connected',
        copy: 'The service restarted too many times, so Faktor stopped trying. Check Diagnostics, then try again.',
        actions: [
          { key: 'retryStart', label: 'Try again' },
          { key: 'openDiagnostics', label: 'Open Diagnostics' },
        ],
        detail,
      };
    case 'configuration_invalid':
      return {
        kind,
        label: 'Faktor’s service settings are invalid',
        copy: 'Fix the service configuration in Diagnostics, then try again.',
        actions: [{ key: 'openDiagnostics', label: 'Open Diagnostics' }],
        detail,
      };
    case 'workspace_untrusted':
      return {
        kind,
        label: 'Faktor is paused in this workspace',
        copy: 'This workspace is not trusted, so Faktor did not start its service.',
        actions: [
          { key: 'manageWorkspaceTrust', label: 'Manage Workspace Trust' },
          { key: 'openDiagnostics', label: 'Open Diagnostics' },
        ],
        detail,
      };
    case 'workspace_unavailable':
      return {
        kind,
        label: 'Open a project',
        copy: 'Open a project folder so Faktor knows where to work.',
        actions: [{ key: 'openProject', label: 'Open project' }],
        detail,
      };
    case 'user_action_required':
      return {
        kind,
        label: 'Faktor needs you',
        copy: 'Faktor cannot continue without a decision. Open Diagnostics to see the exact cause.',
        actions: [{ key: 'openDiagnostics', label: 'Open Diagnostics' }],
        detail,
      };
  }
}

/** True when `child` is `parent` or lives below it (canonicalized). */
export function isPathInside(parent: string, child: string): boolean {
  const root = resolve(parent);
  const target = resolve(child);
  const rel = relative(root, target);
  return rel.length === 0 || (!rel.startsWith('..') && !isAbsolute(rel));
}

export interface ServicePlanGateInput {
  readonly plan: ServiceLaunchPlan;
  readonly mode: ServiceMode;
  /** Automatic starts/restarts require a trusted workspace. */
  readonly workspaceTrusted: boolean;
  readonly workspaceRoot: string;
  readonly allowUnverified: boolean;
}

/**
 * The ONE pre-launch gate. It never spawns; it returns the typed authority
 * that explains why a start must not happen (or null when the plan is safe).
 * Automatic/restart starts additionally require the executable to come from
 * the trusted install (the signed layout launcher, or a path inside its
 * install root); repository paths are refused in every mode.
 */
export function servicePlanAuthority(input: ServicePlanGateInput): ServiceAuthority | null {
  const { plan, mode } = input;
  if (mode !== 'explicit' && !input.workspaceTrusted) {
    return serviceAuthority(
      'workspace_untrusted',
      'the workspace is not trusted; automatic startup is disabled',
    );
  }
  if (isPathInside(input.workspaceRoot, plan.executable)) {
    return serviceAuthority(
      'outside_trusted_install',
      `${plan.executable} is inside the workspace (${input.workspaceRoot}); repository-supplied executables are never started`,
    );
  }
  if (mode !== 'explicit' && plan.source !== 'install-layout') {
    if (plan.source === 'environment' || !isPathInside(plan.installRoot, plan.executable)) {
      return serviceAuthority(
        'outside_trusted_install',
        plan.source === 'environment'
          ? `${plan.executable} came from the process environment; only a machine-scoped installed executable starts automatically`
          : `${plan.executable} is outside the trusted install ${plan.installRoot}; only machine-scoped installed executables start automatically`,
      );
    }
  }
  if (plan.trust.state === 'refused') {
    return serviceAuthority('trust_changed', plan.trust.detail);
  }
  if (plan.trust.state === 'unverified' && !(mode === 'explicit' && input.allowUnverified)) {
    return serviceAuthority('trust_unverified', plan.trust.detail);
  }
  return null;
}

export interface ServiceConfigurationInput {
  readonly binaryPath: string;
  readonly dataDir: string;
  readonly installRoot: string;
  readonly startupTimeoutMs: number;
  readonly extraArgs: readonly string[];
}

/** Validate the machine-scoped service settings; null means valid. */
export function validateServiceConfiguration(input: ServiceConfigurationInput): string | null {
  for (const [key, value] of [
    ['faktor.binaryPath', input.binaryPath],
    ['faktor.dataDir', input.dataDir],
    ['faktor.installRoot', input.installRoot],
  ] as const) {
    if (value.length > 0 && !isAbsolute(value)) {
      return `${key} must be an absolute path (received ${JSON.stringify(value)})`;
    }
    if (value.length > 4096) {
      return `${key} is longer than the 4096-character bound`;
    }
    if (value.includes('\u0000')) {
      return `${key} contains a NUL byte`;
    }
  }
  if (
    !Number.isInteger(input.startupTimeoutMs) ||
    input.startupTimeoutMs < 1_000 ||
    input.startupTimeoutMs > 600_000
  ) {
    return 'faktor.startupTimeoutMs must be an integer between 1000 and 600000 milliseconds';
  }
  if (!Array.isArray(input.extraArgs)) {
    return 'faktor.extraArgs must be an array of strings';
  }
  if (input.extraArgs.length > 64) {
    return 'faktor.extraArgs may carry at most 64 arguments';
  }
  for (const arg of input.extraArgs) {
    if (typeof arg !== 'string' || arg.length > 4096 || arg.includes('\u0000')) {
      return 'faktor.extraArgs entries must be bounded strings without NUL bytes';
    }
  }
  return null;
}

/** Classify a launch rejection that carries an authority meaning. */
export function launchFailureAuthority(error: unknown): ServiceAuthorityKind | null {
  const message = error instanceof Error ? error.message : String(error);
  if (/checksum|signature|digest|release trust|trusted release|tamper|code=3\b/i.test(message)) {
    return 'trust_changed';
  }
  if (/ENOENT|not found|binary missing/i.test(message)) {
    return 'binary_missing';
  }
  if (/configuration|invalid setting/i.test(message)) {
    return 'configuration_invalid';
  }
  return null;
}

export class ServiceLifecycle<C extends ServiceConnection> {
  private readonly deps: ServiceLifecycleDeps<C>;
  private readonly config: ServiceLifecycleConfig;
  private readonly now: () => number;
  private readonly setTimer: (callback: () => void, ms: number) => unknown;
  private readonly clearTimer: (handle: unknown) => void;

  private phase: ServicePhase = 'idle';
  private connection: C | null = null;
  private plan: ServiceLaunchPlan | null = null;
  private authority: ServiceAuthority | null = null;
  private notice: ConnectionNotice = null;
  private lastFailure: string | null = null;
  private readinessDetail: string | null = null;
  private restarts: number[] = [];
  private generation = 0;
  private resumedGeneration = -1;
  private launching: Promise<void> | null = null;
  private controller: AbortController | null = null;
  private stoppedByUser = false;
  private disposed = false;
  private openRequested = false;
  private connectingTimer: unknown = null;
  private restartTimer: unknown = null;
  private probeTimer: unknown = null;
  private stableTimer: unknown = null;

  constructor(deps: ServiceLifecycleDeps<C>) {
    this.deps = deps;
    this.config = { ...DEFAULT_SERVICE_CONFIG, ...deps.config };
    this.now = deps.now ?? (() => Date.now());
    this.setTimer = deps.setTimeout ?? ((callback, ms) => setTimeout(callback, ms));
    this.clearTimer = deps.clearTimeout ?? ((handle) => clearTimeout(handle as NodeJS.Timeout));
  }

  view(): ServiceLifecycleView {
    return {
      phase: this.phase,
      connectionNotice: this.notice,
      authority: this.authority,
      diagnostics: {
        phase: this.phase,
        executable: this.plan?.executable ?? null,
        source: this.plan?.source ?? null,
        trust: this.plan?.trust.state ?? null,
        readiness:
          this.phase === 'running'
            ? 'ready'
            : this.phase === 'connecting' || this.phase === 'reconnecting'
              ? 'waiting'
              : 'idle',
        readinessDetail: this.readinessDetail,
        port: this.connection?.port ?? null,
        pid: this.connection?.pid ?? null,
        baseUrl: this.connection?.baseUrl ?? null,
        restarts: this.restartTimes().length,
        restartBudget: this.config.maxRestarts,
        lastFailure: this.lastFailure,
      },
    };
  }

  /** Opening Faktor: auto-start once unless explicitly stopped or blocked. */
  async open(): Promise<void> {
    this.openRequested = true;
    if (this.disposed || this.stoppedByUser || this.authority !== null) {
      // Genuine authority is terminal for the automatic path: recovery is an
      // explicit Diagnostics action, never a silent retry on the next open.
      return;
    }
    if (this.launching !== null) {
      return this.launching;
    }
    if (this.connection !== null && (await this.isAlive(this.connection))) {
      return;
    }
    await this.start('auto', { allowUnverified: false });
  }

  /** Diagnostics "Start service": clears the explicit-stop latch and budget. */
  async startExplicit(allowUnverified = false): Promise<void> {
    if (this.disposed) {
      return;
    }
    if (this.connection !== null && (await this.isAlive(this.connection))) {
      this.stoppedByUser = false;
      this.phase = 'running';
      this.authority = null;
      this.notice = null;
      this.publish();
      return;
    }
    this.stoppedByUser = false;
    this.authority = null;
    this.lastFailure = null;
    this.restarts = [];
    this.clearTimerIf(this.restartTimer);
    this.restartTimer = null;
    this.openRequested = true;
    this.publish();
    await this.start('explicit', { allowUnverified });
  }

  /** Diagnostics "Stop service": auto-start/restart stays off until explicit. */
  async stopExplicit(): Promise<void> {
    this.stoppedByUser = true;
    this.clearAllTimers();
    this.controller?.abort();
    const launching = this.launching;
    if (launching !== null) {
      try {
        await launching;
      } catch {
        // The aborted attempt owns its own teardown.
      }
    }
    const connection = this.connection;
    this.connection = null;
    this.phase = 'stopped_by_user';
    this.notice = null;
    this.authority = null;
    this.readinessDetail = null;
    this.publish();
    if (connection !== null) {
      try {
        await this.deps.shutdown(connection, 'stop');
      } catch {
        // Shutdown is best-effort; ownership is already dropped.
      }
    }
    this.publish();
  }

  /** T05 Cancel: abort the in-flight start and leave auto-start off. */
  cancel(): void {
    this.stoppedByUser = true;
    this.clearAllTimers();
    this.controller?.abort();
    this.authority = null;
    this.notice = null;
    this.phase = 'stopped_by_user';
    this.publish();
  }

  /** Window teardown: never leaves an owned process behind. */
  async dispose(): Promise<void> {
    this.disposed = true;
    this.clearAllTimers();
    this.controller?.abort();
    const launching = this.launching;
    if (launching !== null) {
      try {
        await launching;
      } catch {
        // Ignored during teardown.
      }
    }
    const connection = this.connection;
    this.connection = null;
    if (connection !== null) {
      try {
        await this.deps.shutdown(connection, 'dispose');
      } catch {
        // Ignored during teardown.
      }
    }
  }

  private async start(mode: ServiceMode, request: ServiceStartRequest): Promise<void> {
    if (this.disposed || this.stoppedByUser) {
      return;
    }
    if (this.launching !== null) {
      return this.launching;
    }
    if (this.connection !== null && (await this.isAlive(this.connection))) {
      // A stale restart timer (or a concurrent explicit start) must never
      // spawn a second daemon over the live one.
      return;
    }
    const attempt = this.startOnce(mode, request).finally(() => {
      if (this.launching === attempt) {
        this.launching = null;
      }
    });
    this.launching = attempt;
    return attempt;
  }

  private async startOnce(mode: ServiceMode, request: ServiceStartRequest): Promise<void> {
    if (mode !== 'restart') {
      this.authority = null;
    }
    this.phase = mode === 'restart' ? 'reconnecting' : 'connecting';
    if (mode === 'restart') {
      this.notice = 'reconnecting';
    } else {
      this.scheduleConnectingNotice();
    }
    this.publish();

    let preparation: ServicePreparation;
    try {
      preparation = await this.deps.prepare(mode, request);
    } catch (error) {
      this.fail(false, error, mode);
      return;
    }
    if (this.disposed || this.stoppedByUser) {
      return;
    }
    if (!preparation.ok) {
      this.block(preparation.failure.kind, preparation.failure.detail, preparation.failure.retryable === true);
      return;
    }
    const plan = preparation.plan;
    this.plan = plan;
    this.readinessDetail = null;
    const controller = new AbortController();
    this.controller = controller;
    let connection: C;
    try {
      connection = await this.deps.launch(plan, controller.signal);
    } catch (error) {
      this.controller = null;
      if (controller.signal.aborted || this.disposed || this.stoppedByUser) {
        return;
      }
      const kind = launchFailureAuthority(error);
      if (kind !== null) {
        this.block(kind, error instanceof Error ? error.message : String(error), false);
        return;
      }
      this.fail(true, error, mode);
      return;
    }
    this.controller = null;
    if (this.disposed || this.stoppedByUser) {
      try {
        await this.deps.shutdown(connection, 'stop');
      } catch {
        // Teardown is owned by the explicit stop.
      }
      return;
    }
    this.clearTimerIf(this.connectingTimer);
    this.connectingTimer = null;
    this.connection = connection;
    this.generation += 1;
    this.phase = 'running';
    this.notice = null;
    this.authority = null;
    this.lastFailure = null;
    this.readinessDetail = 'GET /native/ready reports ready';
    this.publish();
    this.scheduleProbe();
    this.scheduleStableReset();
    await this.resumeOnce(connection);
  }

  private async resumeOnce(connection: C): Promise<void> {
    if (this.resumedGeneration === this.generation) {
      return;
    }
    this.resumedGeneration = this.generation;
    try {
      await this.deps.resume(connection, this.generation);
    } catch (error) {
      // The daemon is healthy; a resume failure is a session read problem,
      // not a reason to spawn another daemon. Keep the connection.
      this.lastFailure = error instanceof Error ? error.message : String(error);
      this.publish();
    }
  }

  private fail(retryable: boolean, error: unknown, mode: ServiceMode): void {
    const detail = error instanceof Error ? error.message : String(error);
    this.lastFailure = detail;
    if (!retryable) {
      this.block('user_action_required', detail, false);
      return;
    }
    if (this.restartTimes().length >= this.config.maxRestarts) {
      this.block(
        'startup_budget_exhausted',
        `the service failed ${this.restartTimes().length} times within ${this.config.restartWindowMs}ms (last: ${detail})`,
        false,
      );
      return;
    }
    this.restarts.push(this.now());
    this.phase = 'reconnecting';
    this.notice = 'reconnecting';
    this.publish();
    this.scheduleRestart(mode === 'restart' ? 'restart' : 'auto');
  }

  private block(kind: ServiceAuthorityKind, detail: string, retryable: boolean): void {
    this.clearAllTimers();
    this.notice = null;
    this.lastFailure = detail;
    this.readinessDetail = null;
    if (retryable) {
      this.fail(true, new Error(detail), 'auto');
      return;
    }
    this.phase = 'blocked';
    this.authority = serviceAuthority(kind, detail);
    this.publish();
    this.deps.onAuthority?.(this.authority);
  }

  private async onConnectionLost(connection: C): Promise<void> {
    if (this.disposed || this.stoppedByUser || this.connection !== connection) {
      return;
    }
    this.clearTimerIf(this.probeTimer);
    this.clearTimerIf(this.stableTimer);
    this.probeTimer = null;
    this.stableTimer = null;
    this.connection = null;
    this.phase = 'reconnecting';
    this.notice = 'reconnecting';
    this.readinessDetail = null;
    this.publish();
    try {
      await this.deps.shutdown(connection, 'crash');
    } catch {
      // The process is already gone; restart still proceeds.
    }
    if (this.disposed || this.stoppedByUser) {
      return;
    }
    const inWindow = this.restartTimes();
    if (inWindow.length >= this.config.maxRestarts) {
      this.block(
        'startup_budget_exhausted',
        `the service crashed ${inWindow.length} times within ${this.config.restartWindowMs}ms`,
        false,
      );
      return;
    }
    this.restarts.push(this.now());
    this.scheduleRestart('restart');
  }

  private scheduleRestart(mode: 'auto' | 'restart'): void {
    if (this.disposed || this.stoppedByUser || this.restartTimer !== null) {
      return;
    }
    const delay = this.backoffDelay();
    this.restartTimer = this.setTimer(() => {
      this.restartTimer = null;
      if (this.disposed || this.stoppedByUser) {
        return;
      }
      void this.start(mode === 'restart' ? 'restart' : 'auto', { allowUnverified: false });
    }, delay);
  }

  private backoffDelay(): number {
    const exponent = Math.max(0, Math.min(this.restartTimes().length - 1, 10));
    const base = Math.min(this.config.backoffBaseMs * 2 ** exponent, this.config.backoffMaxMs);
    const jitter = Math.max(0, Math.min(1, this.config.jitter()));
    return Math.min(this.config.backoffMaxMs, Math.round(base + base * 0.2 * jitter));
  }

  private scheduleConnectingNotice(): void {
    this.clearTimerIf(this.connectingTimer);
    this.connectingTimer = this.setTimer(() => {
      this.connectingTimer = null;
      if (this.disposed || this.stoppedByUser || this.phase !== 'connecting') {
        return;
      }
      this.notice = 'connecting';
      this.publish();
    }, this.config.connectingNoticeMs);
  }

  private scheduleProbe(): void {
    this.clearTimerIf(this.probeTimer);
    this.probeTimer = this.setTimer(() => {
      this.probeTimer = null;
      const connection = this.connection;
      if (connection === null || this.disposed || this.stoppedByUser) {
        return;
      }
      void this.isAlive(connection).then((alive) => {
        if (connection !== this.connection) {
          return;
        }
        if (alive) {
          this.scheduleProbe();
        } else {
          void this.onConnectionLost(connection);
        }
      });
    }, this.config.probeIntervalMs);
  }

  private scheduleStableReset(): void {
    this.clearTimerIf(this.stableTimer);
    this.stableTimer = this.setTimer(() => {
      this.stableTimer = null;
      if (this.phase === 'running' && this.connection !== null) {
        this.restarts = [];
        this.publish();
      }
    }, this.config.stableResetMs);
  }

  private async isAlive(connection: C): Promise<boolean> {
    try {
      return await this.deps.probe(connection);
    } catch {
      return false;
    }
  }

  private restartTimes(): number[] {
    const cutoff = this.now() - this.config.restartWindowMs;
    this.restarts = this.restarts.filter((at) => at >= cutoff);
    return this.restarts;
  }

  private publish(): void {
    this.deps.publish(this.view());
  }

  private clearAllTimers(): void {
    this.clearTimerIf(this.connectingTimer);
    this.clearTimerIf(this.restartTimer);
    this.clearTimerIf(this.probeTimer);
    this.clearTimerIf(this.stableTimer);
    this.connectingTimer = null;
    this.restartTimer = null;
    this.probeTimer = null;
    this.stableTimer = null;
  }

  private clearTimerIf(handle: unknown): void {
    if (handle !== null && handle !== undefined) {
      this.clearTimer(handle);
    }
  }

  /** Exposed for tests/diagnostics: the last published view. */
  requestedOpen(): boolean {
    return this.openRequested;
  }
}
