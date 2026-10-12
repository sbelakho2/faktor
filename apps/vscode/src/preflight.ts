// The ONE preflight/readiness presentation state (I06) and the stream-recovery
// (I06e/T27/T28) and sandbox-guidance (I10/T32) copy tables.
//
// Dependency-free (no vscode import) so scripts/selftest.mjs can drive the
// derivation adversarially: every state, the D10 bare-checksum wording and
// the exact T00-T05 label/copy/action matrix are asserted from here, and the
// webview renders the SAME view object the extension publishes.
//
// Invariants encoded here:
//  - `refused` NEVER offers a start action: its only action is `Locate
//    daemon…` (the operator must pick a different executable or repair the
//    installation).
//  - `unverified` shows the exact resolved path and both `Start anyway` /
//    `Cancel`; there is no implicit start path.
//  - D10: the bare `<binary>.sha256` check is never described as "signed";
//    the signed-install wording stays scoped to the bootstrap launcher.
//  - `ready` requires an explicit readiness success; a running service whose
//    readiness is still waiting/timed out stays `starting`.

import type { BinaryTrust, ResolvedDaemon } from './daemon';

export type PreflightState =
  | 'workspace_unavailable'
  | 'stopped'
  | 'missing'
  | 'unverified'
  | 'refused'
  | 'starting'
  | 'ready';

export type PreflightActionKey =
  | 'openProject'
  | 'startService'
  | 'locateDaemon'
  | 'startAnyway'
  | 'cancelConfirm'
  | 'cancelStart'
  | 'wait'
  | 'refresh'
  | 'reconnectStream'
  | 'refreshSnapshot'
  | 'openSetupGuidance';

export interface PreflightAction {
  readonly key: PreflightActionKey;
  readonly label: string;
}

/** The exact state-matrix rows (labels/copy/actions are frozen verbatim). */
export interface PreflightCopy {
  readonly state: PreflightState;
  readonly label: string;
  readonly copy: string;
  readonly actions: readonly PreflightAction[];
}

export const T00_WORKSPACE_UNAVAILABLE: PreflightCopy = {
  state: 'workspace_unavailable',
  label: 'Workspace unavailable',
  copy: 'Open a project to choose where this run works.',
  actions: [{ key: 'openProject', label: 'Open project' }],
};

export const T01_STOPPED: PreflightCopy = {
  state: 'stopped',
  label: 'Faktor is stopped',
  copy: 'Start the service when you are ready.',
  actions: [{ key: 'startService', label: 'Start service' }],
};

export const T02_MISSING: PreflightCopy = {
  state: 'missing',
  label: 'Faktor could not find its service',
  copy: 'Locate an application-level executable or install the service.',
  actions: [{ key: 'locateDaemon', label: 'Locate daemon…' }],
};

export const T03_UNVERIFIED: PreflightCopy = {
  state: 'unverified',
  label: 'Checksum record unavailable',
  copy: 'Confirm the exact executable before starting.',
  actions: [
    { key: 'startAnyway', label: 'Start anyway' },
    { key: 'cancelConfirm', label: 'Cancel' },
  ],
};

export const T04_REFUSED: PreflightCopy = {
  state: 'refused',
  label: 'The service executable does not match its record',
  copy: 'Choose another executable or repair the installation.',
  actions: [{ key: 'locateDaemon', label: 'Locate daemon…' }],
};

export const T05_STARTING: PreflightCopy = {
  state: 'starting',
  label: 'Starting Faktor',
  copy: 'Resolving the confirmed executable and waiting for readiness.',
  actions: [{ key: 'cancelStart', label: 'Cancel startup' }],
};

/** The explicit-ready row (no action: starting is already complete). */
export const READY: PreflightCopy = {
  state: 'ready',
  label: 'Faktor is ready',
  copy: 'The service is running and ready for work.',
  actions: [],
};

export const PREFLIGHT_COPY: Record<PreflightState, PreflightCopy> = {
  workspace_unavailable: T00_WORKSPACE_UNAVAILABLE,
  stopped: T01_STOPPED,
  missing: T02_MISSING,
  unverified: T03_UNVERIFIED,
  refused: T04_REFUSED,
  starting: T05_STARTING,
  ready: READY,
};

export type PreflightReadiness = 'idle' | 'waiting' | 'ready' | 'timeout' | 'failed';

export interface PreflightView {
  readonly state: PreflightState;
  readonly label: string;
  readonly copy: string;
  readonly actions: readonly PreflightAction[];
  /** The resolved executable, exactly as it was resolved (never a workspace
   *  target/ fallback: resolution itself refuses those inputs). */
  readonly executable: string | null;
  readonly trust: 'verified' | 'unverified' | 'refused' | null;
  /** The D10 bare-checksum wording, or the launcher-scoped install wording. */
  readonly trustLabel: string | null;
  /** Advanced trust detail (demoted to Diagnostics), or null. */
  readonly trustDetail: string | null;
  readonly readiness: PreflightReadiness;
  readonly readinessDetail: string | null;
  /** The last non-cancel start failure (demoted to Diagnostics), or null. */
  readonly failureDetail: string | null;
}

export interface PreflightInput {
  /** False when this window has no workspace folder at all (T00). */
  readonly workspaceOpen: boolean;
  readonly daemon: 'stopped' | 'starting' | 'running' | 'error';
  /** An explicit start attempt is in flight (T05, cancellable). */
  readonly startingAttempt: boolean;
  readonly resolution: ResolvedDaemon | null;
  readonly trust: BinaryTrust | null;
  /** The actionable resolve refusal (missing binary), or null. */
  readonly resolveError: string | null;
  readonly readiness: PreflightReadiness;
  readonly readinessDetail: string | null;
  /** The last non-cancel start failure detail (when the binary resolved). */
  readonly failureDetail: string | null;
}

/**
 * D10 wording for the bare `.sha256` check:
 *  - `verified` via an existing record -> `Matches recorded checksum`;
 *  - no record -> `No checksum record`;
 *  - refused (mismatch/malformed/unreadable record) -> `Checksum mismatch`.
 * The signed-install layout is the ONE launcher-scoped exception: its
 * verification is performed by the bootstrap launcher, never by this check.
 */
export function bareChecksumTrustLabel(
  trust: Pick<BinaryTrust, 'state' | 'source'>,
): string {
  if (trust.state === 'verified') {
    return trust.source === 'install-layout'
      ? 'Verified by the signed install launcher'
      : 'Matches recorded checksum';
  }
  if (trust.state === 'unverified') {
    return 'No checksum record';
  }
  return 'Checksum mismatch';
}

/**
 * Derive the ONE preflight view. Ordering matters:
 *   1. no workspace folder             -> T00
 *   2. an explicit start in flight     -> T05 (cancellable)
 *   3. running + readiness ready       -> ready
 *   4. running (readiness incomplete)  -> T05 (never implicit-ready)
 *   5. resolution refused              -> T04 (locate only, never start)
 *   6. resolution unverified           -> T03 (exact path + confirm)
 *   7. resolution missing              -> T02
 *   8. resolved + verified             -> T01
 */
export function derivePreflight(input: PreflightInput): PreflightView {
  const executable = input.resolution?.executable ?? null;
  const trust = input.trust?.state ?? null;
  const trustLabel = input.trust !== null ? bareChecksumTrustLabel(input.trust) : null;
  const trustDetail = input.trust?.detail ?? null;
  const base = {
    executable,
    trust,
    trustLabel,
    trustDetail,
    readiness: input.readiness,
    readinessDetail: input.readinessDetail,
    failureDetail: input.failureDetail,
  };
  if (!input.workspaceOpen) {
    return { ...T00_WORKSPACE_UNAVAILABLE, ...base };
  }
  if (input.startingAttempt || input.daemon === 'starting') {
    return { ...T05_STARTING, ...base };
  }
  if (input.daemon === 'running') {
    if (input.readiness === 'ready') {
      return { ...READY, ...base };
    }
    // Liveness is not readiness: a running service that never reported ready
    // stays in the cancellable starting presentation (typed detail in
    // Diagnostics), never an implicit ready.
    return { ...T05_STARTING, ...base };
  }
  if (input.resolveError !== null) {
    return { ...T02_MISSING, ...base };
  }
  if (input.trust === null || input.resolution === null) {
    return { ...T02_MISSING, ...base };
  }
  if (input.trust.state === 'refused') {
    return { ...T04_REFUSED, ...base };
  }
  if (input.trust.state === 'unverified') {
    return { ...T03_UNVERIFIED, ...base };
  }
  return { ...T01_STOPPED, ...base };
}

// ------------------------------------------------------- stream recovery I06e

export type StreamRecoveryState = 'idle' | 'reconnecting' | 'paused';

export interface StreamRecoveryView {
  readonly state: StreamRecoveryState;
  readonly label: string;
  readonly copy: string;
  readonly actions: readonly PreflightAction[];
}

/** T27: an explicit reconnect attempt is under way; the cursor is kept. */
export const T27_RECONNECTING: StreamRecoveryView = {
  state: 'reconnecting',
  label: 'Reconnecting live updates',
  copy: 'The last valid journal position is kept.',
  actions: [
    { key: 'wait', label: 'Wait' },
    { key: 'refresh', label: 'Refresh' },
  ],
};

/** T28: the durable next event cannot be read; nothing is ever skipped. */
export const T28_PAUSED: StreamRecoveryView = {
  state: 'paused',
  label: 'Live updates paused',
  copy: 'The client cannot read the next durable event. Nothing is skipped.',
  actions: [
    { key: 'reconnectStream', label: 'Reconnect' },
    { key: 'refreshSnapshot', label: 'Refresh from snapshot' },
  ],
};

export function streamRecoveryView(state: StreamRecoveryState): StreamRecoveryView {
  if (state === 'reconnecting') {
    return T27_RECONNECTING;
  }
  if (state === 'paused') {
    return T28_PAUSED;
  }
  return { state: 'idle', label: '', copy: '', actions: [] };
}

// ------------------------------------------------------ sandbox guidance I10

export interface SandboxGuidanceView {
  readonly label: string;
  readonly copy: string;
  /** The exact CLI command; copyable, never executed from this panel. */
  readonly command: string;
  readonly action: PreflightAction;
  /** Bounded factual guidance (describes what the CLI records; no claim the
   *  panel cannot back up). */
  readonly guidance: string;
}

export const SANDBOX_GUIDANCE: SandboxGuidanceView = {
  label: 'Shell execution needs a decision',
  copy: 'The secure default remains fail-closed on this platform.',
  command: 'faktor sandbox choose',
  action: { key: 'openSetupGuidance', label: 'Open setup guidance' },
  guidance:
    'Run this on the machine that hosts the Faktor service. The service records the choice and ' +
    'applies it to agent shell and verification children; this panel cannot change shell ' +
    'execution policy.',
};
