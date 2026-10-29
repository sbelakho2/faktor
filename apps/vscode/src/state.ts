// Minimal observable state store for the chat panel plus the transcript
// reducer that turns native message pages and SSE event frames into the
// rendered conversation. Dependency-free (no vscode import) so
// scripts/selftest.mjs can drive it directly.
//
// "Bounded everything": the transcript keeps at most MAX_TRANSCRIPT_ENTRIES
// entries and MAX_ENTRY_CHARS characters per text field; agent summaries
// bound every free-form JSON blob; deltas past the cap are truncated, never
// accumulated forever.

import { emptyUx, projectUx, type UxProjection } from './uxState.ts';
import { foldPixelPresence, pixelPresence } from './pixelAgents.ts';
import type { PixelPresence } from './pixelAgents.ts';
import type { EngineeringContractView } from './contract.ts';
import type { CockpitSection, CockpitTournamentView, CockpitUsagePanel, CockpitView } from './cockpit';
import type { ChangesSummary } from './changeReview.ts';
import type { OutcomesView } from './outcomes.ts';
import type { PreflightView } from './preflight.ts';
import type {
  ConnectionNotice,
  ServiceAuthority,
  ServiceDiagnosticsView,
} from './serviceLifecycle.ts';
import type {
  NativeBoardPage,
  NativeIndexCoverageSnapshot,
  NativePermissionEntry,
} from './nativeClient.ts';

import { safeSlice, stripDisplayControls } from './displayText.ts';

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export const MAX_TRANSCRIPT_ENTRIES = 500;
export const MAX_ENTRY_CHARS = 256 * 1024;

/** Bound of one summary JSON blob (progress/result/capabilities). */
export const MAX_SUMMARY_JSON_CHARS = 8 * 1024;
export const MAX_SUMMARY_ARRAY = 64;
export const MAX_SUMMARY_KEYS = 64;
export const MAX_SUMMARY_STRING = 512;

export interface TranscriptTool {
  readonly toolCallId: string;
  readonly name: string;
  readonly state: string;
  readonly input: Json;
  readonly excerpt: string | null;
  readonly exitCode: number | null;
  readonly artifact: string | null;
}

/**
 * One ordered transcript part (audit item 8): the reducer preserves the
 * ARRIVAL ORDER of text/reasoning/summary segments and tool runs instead of
 * collapsing them into text-then-tools buckets, so a tool result renders
 * between the text before it and the text after it. Tool parts reference
 * the merged tool by id (single source of truth, no duplicated state).
 */
export type TranscriptPart =
  | { readonly kind: 'text' | 'reasoning' | 'summary'; readonly text: string }
  | { readonly kind: 'tool'; readonly toolCallId: string };

export interface TranscriptEntry {
  readonly id: string;
  readonly role: string;
  readonly seq: number;
  readonly createdMs: number;
  readonly text: string;
  readonly reasoning: string;
  readonly summary: string;
  readonly tools: readonly TranscriptTool[];
  /**
   * The ordered typed parts (audit item 8). Entries built by the reducer
   * always carry it; a legacy entry literal (tests/fixtures) may omit it
   * and the renderer falls back to the derived buckets.
   */
  readonly parts?: readonly TranscriptPart[];
  /**
   * TRUE when this entry's text hit the `MAX_ENTRY_CHARS` retained-output
   * boundary (audit item 13): the transcript shows an explicit clip marker
   * instead of a silent truncation.
   */
  readonly clipped: boolean;
}

/**
 * One shipped asset's build provenance (audit item 17): the packaged
 * sha256 from `build-info.json` and whether the INSTALLED bytes still
 * hash to it. A mismatch means the tree was modified after packaging —
 * Diagnostics says so instead of trusting the label.
 */
export interface ProvenanceAsset {
  readonly path: string;
  readonly packagedSha256: string;
  readonly liveSha256: string;
  readonly matches: boolean;
}

export interface ProvenanceView {
  readonly extensionVersion: string;
  /** The git commit the VSIX was packaged from; null when uncommitted. */
  readonly buildCommit: string | null;
  readonly packagedAt: string | null;
  readonly assets: readonly ProvenanceAsset[];
  /** TRUE when every live asset hash still matches the packaged stamp. */
  readonly assetsMatch: boolean;
  readonly daemonVersion: string | null;
  readonly daemonExecutable: string | null;
  /**
   * The daemon's protocol-capability handshake (item 17); null when the
   * serving daemon does not advertise it (degraded by observed routes).
   */
  readonly protocol: {
    readonly liveStreaming: boolean | null;
    readonly attachments: boolean | null;
    readonly turnState: boolean | null;
  } | null;
}

export interface RunSummary {
  readonly taskId: string;
  readonly runId: string;
  readonly mode: string;
  readonly state: string;
  readonly goal: string | null;
  readonly model: string | null;
}

export interface AgentModelInfo {
  readonly provider: string;
  readonly model: string;
  readonly reasoning: boolean;
  readonly thinking: boolean;
  readonly tools: boolean;
}

/**
 * The full child-inspection summary: nothing native is collapsed away.
 * Identity (item/child/worktree/session), ownership, capabilities,
 * progress, result, budget, state, model AND the catalog-derived
 * provider/reasoning/thinking metadata, plus the deterministic pixel
 * avatar for the agent panel. Every free-form field is bounded.
 */
export interface AgentSummary {
  readonly agentId: string;
  readonly kind: string;
  readonly runId: string;
  readonly state: string;
  readonly goal: string;
  readonly model: string | null;
  readonly provider: string | null;
  readonly reasoning: boolean | null;
  readonly thinking: boolean | null;
  readonly itemId: string | null;
  readonly itemKind: string | null;
  readonly itemIds: readonly string[];
  readonly sessionId: number | null;
  readonly worktreeId: number | null;
  readonly ownership: string;
  readonly capabilities: readonly string[];
  readonly progress: Json;
  readonly result: Json;
  readonly budget: number | null;
  readonly blockers: readonly string[];
  /** Durable presentation/attention state; background children render
   * dimmed and tucked. Absent on the wire means foreground. */
  readonly presentation: 'foreground' | 'background';
  readonly pixel: import('./pixelAgents').PixelPresence;
}

export interface TaskPlanStep {
  readonly id: string;
  readonly summary: string;
  readonly state: string;
  readonly dependsOn: readonly string[];
}

export interface BudgetSummary {
  readonly maxTokens: number | null;
  readonly spentTokens: number | null;
  /** Exact micro-USD as canonical decimal strings (never a rounded float). */
  readonly maxCostMicro: string | null;
  readonly spentCostMicro: string;
  readonly openReservedMicro: string;
}

/** One completion-contract step as the UI renders it. */
export interface TaskCompletionStepSummary {
  readonly step: string;
  readonly status: string;
  readonly detail: string | null;
}

/**
 * The PR/CI-fix completion contract of the active task. `source` names the
 * provenance of the step rows:
 *   - `daemon`     — the daemon served the durable per-step rows;
 *   - `derived`    — projected from the durable task-run state and the
 *                    submitted contract (pending, or all-succeeded only
 *                    because the durable gate certified the task);
 *   - `unavailable`— the contract is known but the serving daemon exposes
 *                    no per-step read (terminal non-certified run).
 * Step statuses are NEVER fabricated: a missing read is reported as
 * unavailable, not as success.
 */
export interface TaskCompletionSummary {
  readonly includeCommit: boolean;
  readonly includePush: boolean;
  readonly includePr: boolean;
  readonly steps: readonly TaskCompletionStepSummary[];
  readonly source: 'daemon' | 'derived' | 'unavailable';
  readonly reason: string | null;
}

export interface TaskSummary {
  readonly goal: string;
  readonly state: string;
  readonly completed: readonly string[];
  readonly open: readonly string[];
  readonly testsRun: readonly string[];
  readonly testsFailed: readonly string[];
  readonly changedFiles: readonly string[];
  readonly budget: BudgetSummary | null;
  /** Additive native fields, surfaced when the daemon serves them. */
  readonly acceptanceCriteria: readonly string[];
  readonly plan: readonly TaskPlanStep[];
  readonly blockers: readonly string[];
  readonly evidenceRefs: readonly string[];
  readonly phase: string | null;
  readonly progress: Json;
  /** The Task-mode completion contract + its durable step statuses. */
  readonly completion: TaskCompletionSummary | null;
  /**
   * The durable ENGINEERING contract: constraints, invariants, risk
   * hypotheses and proof obligations with provenance/state/confidence. Null
   * when the serving daemon exposes no contract member (legacy daemon —
   * never fabricated). `contractMalformed`/`contractDroppedRows` disclose a
   * present-but-unreadable member and refused entries.
   */
  readonly contract: EngineeringContractView | null;
  readonly contractMalformed: string | null;
  readonly contractDroppedRows: number;
}


export interface VerificationSummary {
  readonly owed: readonly {
    readonly opId: string;
    readonly tool: string;
    readonly status: string;
    readonly effectStatus: string | null;
  }[];
  readonly failedChecks: readonly { readonly id: string; readonly detail: string }[];
}

export interface UsageSummary {
  readonly tokens: number;
  /** Exact micro-USD as canonical decimal strings (never a rounded float). */
  readonly spentMicro: string;
  readonly maxMicro: string | null;
  readonly openMicro: string;
  readonly truncated: boolean;
}

/**
 * One coordination-board post as the Faktor panel renders it. The board is
 * a run-family durable surface; when the daemon of this compatibility
 * revision serves no board route the snapshot carries `available: false`
 * with the explicit reason — never fabricated posts.
 */
export interface BoardPostSummary {
  readonly id: string;
  readonly author: string;
  readonly subject: string;
  readonly body: string;
  readonly refs: readonly string[];
  readonly revision: number | null;
  readonly createdMs: number | null;
}

export interface BoardStateSummary {
  readonly available: boolean;
  readonly source: string;
  readonly revision: number | null;
  readonly unread: number | null;
  readonly posts: readonly BoardPostSummary[];
  readonly reason: string | null;
}

/** Mirrors of the daemon/bridge board bounds (host-side re-validation). */
export const MAX_BOARD_SUBJECT_BYTES = 512;
export const MAX_BOARD_BODY_BYTES = 16 * 1024;
export const MAX_BOARD_REFS = 32;
export const MAX_BOARD_REF_BYTES = 1024;
export const MAX_BOARD_PAGE = 100;

/** The explicitly UNAVAILABLE board: no posts are ever fabricated. */
export function unavailableBoardState(reason: string, source = 'none'): BoardStateSummary {
  return {
    available: false,
    source,
    revision: null,
    unread: null,
    posts: [],
    reason: reason.length > 240 ? `${reason.slice(0, 240)}…` : reason,
  };
}

/**
 * Project one validated native board page onto the snapshot vocabulary.
 * `seenRevision` is the host's read watermark: posts with a strictly newer
 * revision are unread. The watermark only moves on an explicit read/post —
 * automatic refreshes never mark posts read. The returned `watermark` is
 * the board revision at page-read time (resets consume a revision too).
 */
export function boardStateFromPage(
  page: NativeBoardPage,
  seenRevision: number | null,
): { readonly board: BoardStateSummary; readonly watermark: number } {
  const seen = seenRevision ?? 0;
  let unread = 0;
  for (const post of page.posts) {
    if (post.revision > seen) {
      unread += 1;
    }
  }
  return {
    board: {
      available: true,
      source: 'native',
      revision: page.revision,
      unread,
      posts: page.posts.slice(0, MAX_BOARD_PAGE).map((post) => ({
        id: String(post.id),
        author: post.author_child === null ? 'root' : `child:${post.author_child}`,
        subject: boundText(post.subject, MAX_BOARD_SUBJECT_BYTES),
        body: boundText(post.body, MAX_BOARD_BODY_BYTES),
        refs: post.refs.slice(0, MAX_BOARD_REFS).map((ref) => boundText(ref, MAX_BOARD_REF_BYTES)),
        revision: post.revision,
        createdMs: post.created_ms,
      })),
      reason: null,
    },
    watermark: page.revision,
  };
}

function boundText(value: string, maxBytes: number): string {
  const clean = stripDisplayControls(value);
  if (Buffer.byteLength(clean, 'utf8') <= maxBytes) {
    return clean;
  }
  // Byte-accurate, code-point-iterating truncation: slicing `maxBytes`
  // CHARACTERS kept up to 4x the byte bound for multibyte text and could
  // split a surrogate pair. The ellipsis bytes are reserved so the result
  // honors the byte bound exactly.
  const budget = maxBytes > 3 ? maxBytes - 3 : maxBytes;
  let bytes = 0;
  let end = 0;
  for (const ch of clean) {
    const size = Buffer.byteLength(ch, 'utf8');
    if (bytes + size > budget) {
      break;
    }
    bytes += size;
    end += ch.length;
  }
  return `${clean.slice(0, end)}…`;
}

/** One bounded `boardRead` host request, or a typed refusal reason. */
export function parseBoardReadRequest(
  rawSince: unknown,
  rawLimit: unknown,
): { readonly since: number | null; readonly limit: number | null } | { readonly reason: string } {
  let since: number | null = null;
  if (rawSince !== undefined && rawSince !== null) {
    if (
      typeof rawSince !== 'number' ||
      !Number.isInteger(rawSince) ||
      rawSince <= 0 ||
      !Number.isSafeInteger(rawSince)
    ) {
      return { reason: 'boardRead.since must be a positive integer cursor' };
    }
    since = rawSince;
  }
  let limit: number | null = null;
  if (rawLimit !== undefined && rawLimit !== null) {
    if (
      typeof rawLimit !== 'number' ||
      !Number.isInteger(rawLimit) ||
      rawLimit <= 0 ||
      rawLimit > MAX_BOARD_PAGE
    ) {
      return { reason: `boardRead.limit must be an integer in 1..${MAX_BOARD_PAGE}` };
    }
    limit = rawLimit;
  }
  return { since, limit };
}

/**
 * One bounded `boardPost` host request, or a typed refusal reason. The
 * durable authority additionally requires a non-empty subject AND body;
 * refusing here keeps the failure local and explicit instead of a raw 400.
 */
export function parseBoardPostRequest(
  raw: { readonly subject?: unknown; readonly body?: unknown; readonly refs?: unknown },
): { readonly subject: string; readonly body: string; readonly refs: string[] } | { readonly reason: string } {
  const subject = typeof raw.subject === 'string' ? raw.subject.trim() : '';
  if (subject.length === 0) {
    return { reason: 'boardPost.subject must be a non-empty string' };
  }
  if (Buffer.byteLength(subject, 'utf8') > MAX_BOARD_SUBJECT_BYTES) {
    return { reason: `boardPost.subject exceeds ${MAX_BOARD_SUBJECT_BYTES} bytes` };
  }
  if (typeof raw.body !== 'string') {
    return { reason: 'boardPost.body must be a string' };
  }
  const body = raw.body;
  if (body.trim().length === 0) {
    return { reason: 'boardPost.body must be non-empty' };
  }
  if (Buffer.byteLength(body, 'utf8') > MAX_BOARD_BODY_BYTES) {
    return { reason: `boardPost.body exceeds ${MAX_BOARD_BODY_BYTES} bytes` };
  }
  const refs: string[] = [];
  if (raw.refs !== undefined && raw.refs !== null) {
    if (!Array.isArray(raw.refs)) {
      return { reason: 'boardPost.refs must be an array of strings' };
    }
    if (raw.refs.length > MAX_BOARD_REFS) {
      return { reason: `boardPost.refs exceeds ${MAX_BOARD_REFS} entries` };
    }
    for (let index = 0; index < raw.refs.length; index += 1) {
      const ref = raw.refs[index];
      if (typeof ref !== 'string' || ref.trim().length === 0) {
        return { reason: `boardPost.refs[${index}] must be a non-empty string` };
      }
      if (Buffer.byteLength(ref, 'utf8') > MAX_BOARD_REF_BYTES) {
        return { reason: `boardPost.refs[${index}] exceeds ${MAX_BOARD_REF_BYTES} bytes` };
      }
      refs.push(ref.trim());
    }
  }
  return { subject, body, refs };
}

export interface SessionSummary {
  readonly id: string;
  readonly title: string;
  readonly provider: string;
  readonly model: string;
  readonly state: string;
  /**
   * The durable creation time the listing serves (epoch ms); `null`/absent
   * when the serving daemon does not carry it. The History surface renders
   * the real date or an explicit absence — never a fabricated one.
   */
  readonly createdMs?: number | null;
}

/** Bound of the pending-permission card list and its free-form detail. */
export const MAX_PERMISSION_SUMMARIES = 16;
export const MAX_PERMISSION_DETAIL_CHARS = 2000;

/**
 * One live pending permission as the panel renders it. The card is the
 * ATTENTION surface (the request blocks the run); ids/capability/session are
 * disclosed behind "View details" — never headline copy.
 */
export interface PermissionSummary {
  readonly id: string;
  readonly sessionId: string;
  readonly capability: string;
  readonly detail: string;
}

/** Project the strict native pending list onto the bounded panel vocabulary. */
export function permissionSummaries(
  entries: readonly NativePermissionEntry[],
): PermissionSummary[] {
  return entries.slice(0, MAX_PERMISSION_SUMMARIES).map((entry) => ({
    id: entry.id,
    sessionId: stripDisplayControls(entry.sessionId),
    capability: boundedDisplay(entry.capability, 256),
    detail: boundedDisplay(entry.detail, MAX_PERMISSION_DETAIL_CHARS),
  }));
}

/**
 * Approval-card display text: controls and bidi overrides are stripped before
 * truncation so agent-supplied capability/detail text cannot visually spoof
 * the request a human is about to approve.
 */
function boundedDisplay(value: string, max: number): string {
  const clean = stripDisplayControls(value);
  return clean.length > max ? `${safeSlice(clean, max)}…` : clean;
}

export type DaemonStatus = 'stopped' | 'starting' | 'running' | 'error';

/** Run states that are terminal: any other tag is still active. */
export const TERMINAL_RUN_STATES: readonly string[] = ['done', 'failed', 'cancelled'];

export function isTerminalRunState(state: string): boolean {
  return TERMINAL_RUN_STATES.includes(state.trim().toLowerCase());
}

/**
 * The active-run id after one refresh: the tracked id survives ONLY while
 * its run is present AND non-terminal. A terminal run (Done/Failed/
 * Cancelled) clears busy/activeRunId — mere presence in the listing is not
 * liveness.
 */
export function activeRunIdAfter(
  activeRunId: string | null,
  runs: readonly RunSummary[],
): string | null {
  if (activeRunId === null) {
    return null;
  }
  const tracked = runs.find((run) => run.runId === activeRunId);
  if (!tracked || isTerminalRunState(tracked.state)) {
    return null;
  }
  return activeRunId;
}

/**
 * The run a cancel action may target: the tracked run when it is still
 * non-terminal, otherwise the first non-terminal run, otherwise null.
 * Terminal runs are NEVER cancelled (the server would refuse with a typed
 * 409; the client must not even attempt it).
 */
export function cancelRunTarget(
  activeRunId: string | null,
  runs: readonly RunSummary[],
): string | null {
  if (activeRunId !== null) {
    const tracked = runs.find((run) => run.runId === activeRunId);
    if (tracked) {
      return isTerminalRunState(tracked.state) ? null : tracked.runId;
    }
  }
  const live = runs.find((run) => !isTerminalRunState(run.state));
  return live ? live.runId : null;
}


export interface FaktorSnapshot {
  readonly daemon: DaemonStatus;
  readonly daemonDetail: string;
  readonly baseUrl: string | null;
  readonly session: SessionSummary | null;
  readonly machineState: string;
  readonly machineLabel: string;
  readonly sessions: readonly SessionSummary[];
  /**
   * The refusal text of the last durable session-list read (null = the last
   * read was served or none happened yet). When set, the History surface
   * renders its explicit unavailable state (T31 copy) and NO rows.
   */
  readonly sessionsUnavailable: string | null;
  /**
   * The workspace name of this window (the first folder's name), or null.
   * The Inspect breadcrumb scope uses it; session/run ids stay advanced
   * diagnostics, never the primary label.
   */
  readonly workspaceName: string | null;
  readonly runs: readonly RunSummary[];
  readonly activeRunId: string | null;
  /**
   * The full durable task state embedded in the served proof
   * (`GET /native/tasks/{id}/proof` body `task.state`, e.g. `verifying` /
   * `needs_verification`): the only served source of the T12 run strip.
   * Null when no proof is served or readable — the collapsed session tag
   * (`running|in_progress|done|idle`) can never produce this value.
   */
  readonly proofTaskState: string | null;
  readonly agents: readonly AgentSummary[];
  /** The live pending permission requests of the active session. They are
   * the operation's ATTENTION gate: one approval card in the main flow, and
   * an empty list means nothing is waiting on the operator. */
  readonly permissions: readonly PermissionSummary[];
  readonly task: TaskSummary | null;
  readonly verification: VerificationSummary | null;
  readonly usage: UsageSummary | null;
  /** The commercial-metering panel (usage fold + credits + entitlements);
   * null until a read happens, an explicit disabled/unavailable state when
   * the control plane refuses (never fabricated numbers). */
  readonly usagePanel: CockpitUsagePanel | null;
  /** The persistent Task cockpit assembled from every native section. */
  readonly cockpit: CockpitView | null;
  readonly cockpitSections: readonly CockpitSection[];
  /** The durable tournament of the session (proposal only; never auto-merged). */
  readonly tournament: CockpitTournamentView | null;
  /** The run-family coordination board surface (explicitly unavailable when
   * the serving daemon exposes no board read). */
  readonly board: BoardStateSummary | null;
  /**
   * I03: the served candidate-change view (projection filesChanged + refs
   * from served proof/verification relationships). Null before the first
   * read; an explicit unavailable block on a failed projection read.
   */
  readonly changes: ChangesSummary | null;
  /**
   * I07: the lazily read Engineering Outcome Ledger view (History /
   * Outcomes). Null until the operator opens the Outcomes tab; an explicit
   * `Cost unavailable` state on a failed read (never stale numbers).
   */
  readonly outcomes: OutcomesView | null;
  /** Durable index coverage of the session's workspace (audits 5/6): a
   * PARTIAL generation and a stale-while-rebuilding index are named, never
   * flattened into "ready". Null until a read happens or when the daemon
   * never hosted an index service. */
  readonly indexCoverage: NativeIndexCoverageSnapshot | null;
  readonly transcript: readonly TranscriptEntry[];
  /**
   * TRUE when the daemon serves OLDER history than the accumulated
   * transcript (audit item 13): the panel offers an explicit scrollback
   * load whose prepend keeps a stable anchor on the entry being read.
   */
  readonly transcriptHasMore: boolean;
  /** The build provenance + protocol handshake of the running pair. */
  readonly provenance: ProvenanceView | null;
  /**
   * Content digest of `transcript` (host-computed, bounded refresh cadence).
   * The webview skips transcript key work and DOM mutation entirely while it
   * is unchanged; a changed digest with identical lengths still re-keys the
   * entries so same-length edits are never mistaken for a no-op.
   */
  readonly transcriptRevision: string;
  readonly streamStatus: string;
  /**
   * Transient normal-UI connection notice only ("Connecting…"/"Reconnecting…").
   * Null while healthy: the normal surface never shows service state.
   */
  readonly connectionNotice: ConnectionNotice;
  /**
   * Genuine authority that requires the operator (missing binary, changed
   * identity, exhausted restart budget, invalid configuration, ...). The
   * normal surface renders exactly this row; null means stay quiet.
   */
  readonly serviceAuthority: ServiceAuthority | null;
  /** Diagnostics → Service rows (path, trust, readiness, port, pid, restarts). */
  readonly service: ServiceDiagnosticsView | null;
  /**
   * The ONE preflight/readiness presentation state (I06): the same view the
   * Work empty-state card and the Diagnostics mirror render. Null only
   * before the host publishes it (the panel then renders no card and never
   * fabricates a state).
   */
  readonly preflight: PreflightView | null;
  readonly lastError: string | null;
  /**
   * The kind of the failure `lastError` carries (audit item 11): a SERVICE
   * recovery (reconnect, restart) may clear a service/stream-class error but
   * NEVER a turn-scoped one (`run_failure`, `conversation_missing`) — the
   * turn's own outcome must stay visible until its surface retires it.
   * Null when no error is recorded.
   */
  readonly lastErrorKind: string | null;
  readonly busy: boolean;
  /** Explicit Account/Usage surface state; never read by a run refresh. */
  readonly accountStatus: 'not_read' | 'not_connected' | 'read' | 'unavailable';
  /** The shared composer/run UX projection (both hosts use this vocabulary). */
  readonly ux: UxProjection;
}

export function emptySnapshot(): FaktorSnapshot {
  return {
    daemon: 'stopped',
    daemonDetail: '',
    baseUrl: null,
    session: null,
    machineState: 'unknown',
    machineLabel: 'Daemon stopped',
    sessions: [],
    sessionsUnavailable: null,
    workspaceName: null,
    runs: [],
    activeRunId: null,
    proofTaskState: null,
    agents: [],
    task: null,
    verification: null,
    usage: null,
    usagePanel: null,
    cockpit: null,
    cockpitSections: [],
    tournament: null,
    board: null,
    changes: null,
    outcomes: null,
    indexCoverage: null,
    permissions: [],
    transcript: [],
    transcriptRevision: '',
    transcriptHasMore: false,
    provenance: null,
    streamStatus: 'stopped',
    connectionNotice: null,
    serviceAuthority: null,
    service: null,
    preflight: null,
    lastError: null,
    lastErrorKind: null,
    busy: false,
    accountStatus: 'not_read',
    ux: emptyUx(),
  };
}

export type StoreListener = (snapshot: FaktorSnapshot) => void;

export class FaktorStore {
  private state: FaktorSnapshot = emptySnapshot();
  private readonly listeners = new Set<StoreListener>();

  subscribe(listener: StoreListener): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  snapshot(): FaktorSnapshot {
    return this.state;
  }

  patch(patch: Partial<FaktorSnapshot>): void {
    let changed = false;
    const next: Record<string, unknown> = { ...this.state };
    for (const [key, value] of Object.entries(patch)) {
      if (value !== undefined && next[key] !== value) {
        next[key] = value;
        changed = true;
      }
    }
    if (!changed) {
      return;
    }
    const previous = this.state;
    const nextState = next as unknown as FaktorSnapshot;
    // Additive Faktor-only panels never bleed across a session switch or a
    // daemon stop: they describe ONE session/run family. The host patches a
    // fixed field set, so the store owns this invariant (extension.ts is
    // deliberately not coupled to the panel fields).
    const sessionChanged =
      (nextState.session?.id ?? null) !== (previous.session?.id ?? null);
    const stopped = nextState.daemon === 'stopped' && previous.daemon !== 'stopped';
    if (sessionChanged || stopped) {
      next.tournament = null;
      next.board = null;
      next.changes = null;
      next.indexCoverage = null;
      // Pending approvals belong to ONE session/authority: a switch or a
      // stop must never leave a stale approval card actionable.
      next.permissions = [];
    }
    // The usage/credits panel is ORGANIZATION-scoped (not session-owned): it
    // survives a session switch and is dropped only when the daemon stops.
    // The outcome ledger view is DAEMON-scoped the same way.
    if (stopped) {
      next.usagePanel = null;
      next.outcomes = null;
      next.accountStatus = 'not_read';
    }
    // The shared UX projection is derived from the same served fields on
    // every patch, so the webview never re-invents labels or attention rules.
    next.ux = projectUx(nextState as FaktorSnapshot);
    this.state = nextState;
    this.emit();
  }

  private emit(): void {
    for (const listener of [...this.listeners]) {
      listener(this.state);
    }
  }
}

// ------------------------------------------------------- transcript reducer

function clampText(existing: string, addition: string): string {
  if (existing.length >= MAX_ENTRY_CHARS) {
    return existing;
  }
  return safeSlice(existing + addition, MAX_ENTRY_CHARS);
}

function emptyEntry(id: string, role: string, seq: number, createdMs: number): TranscriptEntry {
  return { id, role, seq, createdMs, text: '', reasoning: '', summary: '', tools: [], parts: [], clipped: false };
}

function asObject(value: Json | undefined): Record<string, Json> | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    return null;
  }
  return value;
}

function asString(value: Json | undefined): string | null {
  return typeof value === 'string' ? value : null;
}

function asInt(value: Json | undefined): number | null {
  return typeof value === 'number' && Number.isInteger(value) ? value : null;
}

/** Message ids are integers on durable pages; a string id is tolerated. */
function asId(value: Json | undefined): string | null {
  if (typeof value === 'string' && value.length > 0) {
    return value;
  }
  if (typeof value === 'number' && Number.isInteger(value)) {
    return String(value);
  }
  return null;
}

/** Normalize an SSE `Part` (`{type: ...}`) into a tool update, or null. */
function toolFromSsePart(part: Record<string, Json>): Partial<TranscriptTool> | null {
  const type = asString(part.type);
  if (type === 'tool_call') {
    return {
      toolCallId: asString(part.tool_call_id) ?? '',
      name: asString(part.name) ?? '',
      state: asString(part.state) ?? 'unknown',
      input: part.input ?? null,
    };
  }
  if (type === 'tool_result') {
    const result = asObject(part.result);
    return {
      toolCallId: asString(part.tool_call_id) ?? '',
      excerpt: result ? asString(result.excerpt) : null,
      exitCode: result ? asInt(result.exit_code) : null,
      artifact: result ? asString(result.artifact) : null,
    };
  }
  return null;
}

/** Normalize a durable part row (`{kind, data}` with a flat payload). */
function toolFromDurablePart(
  kind: string,
  payload: Record<string, Json>,
): Partial<TranscriptTool> | null {
  if (kind === 'tool_call') {
    return {
      toolCallId: asString(payload.tool_call_id) ?? '',
      name: asString(payload.name) ?? '',
      state: asString(payload.state) ?? 'unknown',
      input: payload.input ?? null,
    };
  }
  if (kind === 'tool_result') {
    return {
      toolCallId: asString(payload.tool_call_id) ?? '',
      excerpt: asString(payload.excerpt),
      exitCode: asInt(payload.exit_code),
      artifact: asString(payload.artifact),
    };
  }
  return null;
}

function mergeTool(
  tools: readonly TranscriptTool[],
  update: Partial<TranscriptTool>,
): TranscriptTool[] {
  const id = update.toolCallId ?? '';
  const existing = tools.find((tool) => tool.toolCallId === id);
  if (!existing) {
    const created: TranscriptTool = {
      toolCallId: id,
      name: update.name ?? '',
      state: update.state ?? 'running',
      input: update.input ?? null,
      excerpt: update.excerpt ?? null,
      exitCode: update.exitCode ?? null,
      artifact: update.artifact ?? null,
    };
    return [...tools, created];
  }
  const merged: TranscriptTool = {
    toolCallId: existing.toolCallId,
    name: update.name ?? existing.name,
    state: update.state ?? existing.state,
    input: update.input !== undefined ? update.input : existing.input,
    excerpt: update.excerpt !== undefined ? update.excerpt : existing.excerpt,
    exitCode: update.exitCode !== undefined ? update.exitCode : existing.exitCode,
    artifact: update.artifact !== undefined ? update.artifact : existing.artifact,
  };
  return tools.map((tool) => (tool.toolCallId === id ? merged : tool));
}

/**
 * Re-derive the compatibility buckets (`text`/`reasoning`/`summary`/
 * `tools`) from the ordered parts — the parts stay the authority for
 * content and order, the buckets feed the keys/live overlay/legacy
 * consumers.
 */
function withDerivedBuckets(entry: TranscriptEntry): TranscriptEntry {
  const parts = entry.parts ?? [];
  let text = '';
  let reasoning = '';
  let summary = '';
  const tools: TranscriptTool[] = [];
  // The merged tool rows ride in (applyToolUpdate passes the merged
  // array); the derivation only re-orders them to match the parts.
  const toolsById = new Map<string, TranscriptTool>(
    entry.tools.map((tool) => [tool.toolCallId, tool]),
  );
  for (const part of parts) {
    if (part.kind === 'tool') {
      const tool = toolsById.get(part.toolCallId);
      if (tool !== undefined) {
        tools.push(tool);
      }
      continue;
    }
    if (part.kind === 'reasoning') {
      reasoning += part.text;
    } else if (part.kind === 'summary') {
      summary += part.text;
    } else {
      text += part.text;
    }
  }
  return { ...entry, text, reasoning, summary, tools };
}

/**
 * Append ONE text-kind segment to the ordered parts (audit item 8).
 * Consecutive segments of the same kind merge into the last part so the
 * streaming path grows one part per kind run; a different kind (or any tool
 * part between) starts a NEW part — the interleaving survives.
 */
function appendTextPartToParts(
  parts: readonly TranscriptPart[],
  kind: 'text' | 'reasoning' | 'summary',
  text: string,
): TranscriptPart[] {
  const last = parts.length > 0 ? parts[parts.length - 1] : undefined;
  if (last !== undefined && last.kind === kind) {
    return [...parts.slice(0, -1), { kind, text: last.text + text }];
  }
  return [...parts, { kind, text }];
}

function applyTextPart(
  entry: TranscriptEntry,
  kind: string,
  text: string,
): TranscriptEntry {
  const partKind: 'text' | 'reasoning' | 'summary' =
    kind === 'reasoning' ? 'reasoning' : kind === 'summary' ? 'summary' : 'text';
  const bucket =
    partKind === 'reasoning' ? entry.reasoning : partKind === 'summary' ? entry.summary : entry.text;
  // The retained-output boundary (item 13): a bucket grows to
  // MAX_ENTRY_CHARS exactly as before, and the clip becomes EXPLICIT — the
  // entry carries the marker instead of a silent truncation.
  const clamped = clampText(bucket, text);
  const added = clamped.slice(bucket.length);
  const clipped = entry.clipped || added.length < text.length;
  if (added.length === 0) {
    return entry.clipped === clipped ? entry : { ...entry, clipped };
  }
  const parts = appendTextPartToParts(entry.parts ?? [], partKind, added);
  return withDerivedBuckets({ ...entry, parts, clipped });
}

function entryFromMessage(id: string, message: Record<string, Json>): TranscriptEntry {
  let entry = emptyEntry(
    id,
    asString(message.role) ?? 'assistant',
    asInt(message.seq) ?? 0,
    asInt(message.created_ms) ?? 0,
  );
  // User prompt rows carry their text in the message payload (`data.text`,
  // e.g. `{files, text}`) with an empty parts list; assistant rows carry it
  // in parts. Both must render (parts win when both exist).
  const data = asObject(message.data);
  const dataText = data ? asString(data.text) : null;
  const parts = Array.isArray(message.parts) ? (message.parts as Json[]) : [];
  for (const rawPart of parts) {
    const part = asObject(rawPart);
    if (!part) {
      continue;
    }
    const type = asString(part.type) ?? asString(part.kind);
    // Durable part rows carry the flat payload under `data`
    // (`{kind: "text", data: {text}}`); SSE parts carry it inline.
    const payload = asObject(part.data);
    if (type === 'text' || type === 'reasoning' || type === 'summary') {
      const text = asString(part.text) ?? (payload ? asString(payload.text) : null) ?? '';
      if (text.length > 0) {
        entry = applyTextPart(entry, type, text);
      }
      continue;
    }
    const update =
      asString(part.type) !== null
        ? toolFromSsePart(part)
        : toolFromDurablePart(type ?? '', payload ?? part);
    if (update) {
      entry = applyToolUpdate(entry, update);
    }
  }
  if (entry.text.length === 0 && dataText !== null && dataText.length > 0) {
    entry = applyTextPart(entry, 'text', dataText);
  }
  return entry;
}

/**
 * Merge ONE tool update (call or result) into the entry: the tool row
 * merges by `toolCallId` AND keeps its ORDERED PART POSITION — a result
 * that arrives after later text still renders at the call's position (the
 * interleaved order the audit requires), never re-appended at the end.
 */
function applyToolUpdate(
  entry: TranscriptEntry,
  update: Partial<TranscriptTool>,
): TranscriptEntry {
  const tools = mergeTool(entry.tools, update);
  const parts = entry.parts ?? [];
  const id = update.toolCallId ?? '';
  const existingPart = parts.find((part) => part.kind === 'tool' && part.toolCallId === id);
  const nextParts =
    existingPart !== undefined
      ? parts
      : [...parts, { kind: 'tool' as const, toolCallId: id }];
  return withDerivedBuckets({ ...entry, tools, parts: nextParts });
}

function boundTranscript(entries: TranscriptEntry[]): TranscriptEntry[] {
  if (entries.length <= MAX_TRANSCRIPT_ENTRIES) {
    return entries;
  }
  return entries.slice(entries.length - MAX_TRANSCRIPT_ENTRIES);
}

/** Rebuild the transcript from a newest-first native message page. */
export function transcriptFromMessages(messages: readonly Json[]): TranscriptEntry[] {
  // Newest-first input: only the newest MAX_TRANSCRIPT_ENTRIES can survive
  // `boundTranscript`, so never materialize entry objects for the rest. A
  // 100k-message page fed by a hostile/legacy caller costs the same as a
  // 500-message page (the previous loop built all 100k, then discarded
  // 99.5% of them). Order is preserved: slice the newest prefix, then
  // reverse it to oldest-to-newest exactly like the bounded result.
  const newest = messages.length > MAX_TRANSCRIPT_ENTRIES
    ? messages.slice(0, MAX_TRANSCRIPT_ENTRIES)
    : messages;
  const entries: TranscriptEntry[] = [];
  for (const raw of [...newest].reverse()) {
    const message = asObject(raw);
    if (!message) {
      continue;
    }
    const id = asId(message.id);
    if (!id) {
      continue;
    }
    entries.push(entryFromMessage(id, message));
  }
  return boundTranscript(mergeCrossMessageToolResults(entries));
}

/**
 * A denied call's durable result is written as its OWN assistant message
 * (the denial path answers the model without re-opening the tool-call
 * message). The renderer must still show ONE call with its refusal outcome,
 * not a completed call plus an empty "(unnamed tool) exit 1" bubble. Merge
 * by `toolCallId` across adjacent entries and drop a result-only message.
 */
function mergeCrossMessageToolResults(entries: readonly TranscriptEntry[]): TranscriptEntry[] {
  const out = entries.map((entry) => ({ ...entry, tools: [...entry.tools] }));
  const seen = new Map<string, { entry: number; tool: number }>();
  const drop = new Set<number>();
  for (let entryIndex = 0; entryIndex < out.length; entryIndex += 1) {
    const kept: TranscriptTool[] = [];
    for (const tool of out[entryIndex]!.tools) {
      const prior = seen.get(tool.toolCallId);
      if (prior !== undefined && prior.entry !== entryIndex) {
        const target = out[prior.entry]!.tools[prior.tool]!;
        out[prior.entry]!.tools[prior.tool] = {
          ...target,
          name: target.name.length > 0 ? target.name : tool.name,
          excerpt: tool.excerpt ?? target.excerpt,
          exitCode: tool.exitCode ?? target.exitCode,
          artifact: tool.artifact ?? target.artifact,
        };
        continue;
      }
      seen.set(tool.toolCallId, { entry: entryIndex, tool: kept.length });
      kept.push(tool);
    }
    out[entryIndex]!.tools = kept;
    if (
      kept.length === 0 &&
      out[entryIndex]!.text.length === 0 &&
      out[entryIndex]!.reasoning.length === 0 &&
      out[entryIndex]!.summary.length === 0 &&
      out[entryIndex]!.role === 'assistant'
    ) {
      drop.add(entryIndex);
    }
  }
  const kept = out.filter((_, index) => !drop.has(index));
  // Keep the ordered tool PARTS in sync with the merged tool rows (their
  // position is the call's position; only the row content changed).
  return kept.map((entry) => {
    const toolsById = new Map(entry.tools.map((tool) => [tool.toolCallId, tool]));
    const parts = (entry.parts ?? []).map((part) =>
      part.kind === 'tool' && toolsById.has(part.toolCallId)
        ? { kind: 'tool' as const, toolCallId: part.toolCallId }
        : part,
    );
    return { ...entry, parts };
  });
}

/**
 * Content digest of the transcript, not just its shape: two messages with
 * the same lengths but different text must still reach the panel (a
 * length-only signature froze such edits on screen). The digest is computed
 * once per bounded refresh, never per token, and the webview uses it to skip
 * all transcript key work while the stream is idle.
 */
export function transcriptSignature(entries: readonly TranscriptEntry[]): string {
  // 128-bit content digest (four independent FNV-1a lanes). A 32/64-bit
  // signature is cheaply collidable by construction; the webview skips ALL
  // transcript work while its revision is unchanged, so the identity must
  // make a deliberate different-content/same-digest state impractical.
  let laneA = 2166136261;
  let laneB = 2166136261 ^ 0x9e3779b9;
  let laneC = 2166136261 ^ 0x85ebca6b;
  let laneD = 2166136261 ^ 0xc2b2ae35;
  const mix = (text: string, salt: number): void => {
    laneA ^= salt;
    laneA = Math.imul(laneA, 16777619) >>> 0;
    laneB ^= salt ^ 0x9e3779b9;
    laneB = Math.imul(laneB, 16777619) >>> 0;
    laneC ^= salt ^ 0x85ebca6b;
    laneC = Math.imul(laneC, 16777619) >>> 0;
    laneD ^= salt ^ 0xc2b2ae35;
    laneD = Math.imul(laneD, 16777619) >>> 0;
    for (let index = 0; index < text.length; index += 1) {
      const code = text.charCodeAt(index);
      laneA ^= code;
      laneA = Math.imul(laneA, 16777619) >>> 0;
      laneB ^= code + (index & 0xff);
      laneB = Math.imul(laneB, 16777619) >>> 0;
      laneC ^= code ^ (index << 1);
      laneC = Math.imul(laneC, 16777619) >>> 0;
      laneD ^= code + (index * 31);
      laneD = Math.imul(laneD, 16777619) >>> 0;
    }
  };
  for (const entry of entries) {
    mix(entry.id, 1);
    mix(entry.role, 2);
    mix(entry.text, 3);
    mix(entry.reasoning, 4);
    mix(entry.summary, 5);
    mix(entry.clipped ? 'clipped' : '', 11);
    // The ORDERED part sequence (item 8): two transcripts with identical
    // buckets but a different interleaving must never share a digest.
    mix((entry.parts ?? []).map((part) => (part.kind === 'tool' ? `t:${part.toolCallId}` : part.kind)).join('\u0001'), 12);
    for (const tool of entry.tools) {
      mix(tool.state, 6);
      mix(tool.name, 7);
      mix(tool.excerpt ?? '', 8);
      mix(tool.artifact ?? '', 9);
      mix(tool.exitCode === null ? 'n' : String(tool.exitCode), 10);
    }
  }
  const hex = (lane: number): string => (lane >>> 0).toString(16).padStart(8, '0');
  return `${entries.length}:${hex(laneA)}${hex(laneB)}${hex(laneC)}${hex(laneD)}`;
}

/**
 * Exact equality over the fields the transcript panel renders (the same
 * fields `transcriptSignature` mixes). The digest decides whether a patch is
 * WORTH posting; this comparison guarantees a digest collision can never
 * suppress a genuinely different document at the host either. It is O(n)
 * string comparison on the already bounded page, not hashing.
 */
export function transcriptContentEqual(
  left: readonly TranscriptEntry[] | null | undefined,
  right: readonly TranscriptEntry[] | null | undefined,
): boolean {
  if (left === right) {
    return true;
  }
  if (!left || !right || left.length !== right.length) {
    return false;
  }
  for (let index = 0; index < left.length; index += 1) {
    const a = left[index];
    const b = right[index];
    if (
      a.id !== b.id ||
      a.seq !== b.seq ||
      a.role !== b.role ||
      a.text !== b.text ||
      a.reasoning !== b.reasoning ||
      a.summary !== b.summary ||
      a.clipped !== b.clipped
    ) {
      return false;
    }
    const aParts = a.parts ?? [];
    const bParts = b.parts ?? [];
    if (aParts.length !== bParts.length) {
      return false;
    }
    for (let part = 0; part < aParts.length; part += 1) {
      const leftPart = aParts[part];
      const rightPart = bParts[part];
      if (leftPart.kind !== rightPart.kind) {
        return false;
      }
      if (leftPart.kind === 'tool' && rightPart.kind === 'tool') {
        if (leftPart.toolCallId !== rightPart.toolCallId) {
          return false;
        }
      } else if (leftPart.kind !== 'tool' && rightPart.kind !== 'tool') {
        if (leftPart.text !== rightPart.text) {
          return false;
        }
      }
    }
    if (a.tools.length !== b.tools.length) {
      return false;
    }
    for (let tool = 0; tool < a.tools.length; tool += 1) {
      const leftTool = a.tools[tool];
      const rightTool = b.tools[tool];
      if (
        leftTool.state !== rightTool.state ||
        leftTool.name !== rightTool.name ||
        (leftTool.excerpt ?? '') !== (rightTool.excerpt ?? '') ||
        (leftTool.artifact ?? '') !== (rightTool.artifact ?? '') ||
        leftTool.exitCode !== rightTool.exitCode
      ) {
        return false;
      }
    }
  }
  return true;
}

/**
 * Merge ONE fetched history page into the accumulated transcript (audit
 * item 13): entries upsert BY DURABLE MESSAGE ID, order by `seq`, and the
 * accumulated list is never replaced by the newest page alone — scrolling
 * back keeps older entries while a refresh reconciles newer ones. The
 * result stays bounded by MAX_TRANSCRIPT_ENTRIES (the retained tail).
 */
export function mergeTranscriptPages(
  existing: readonly TranscriptEntry[],
  incoming: readonly TranscriptEntry[],
): TranscriptEntry[] {
  const byId = new Map<string, TranscriptEntry>();
  for (const entry of existing) {
    byId.set(entry.id, entry);
  }
  for (const entry of incoming) {
    byId.set(entry.id, entry);
  }
  const merged = Array.from(byId.values()).sort((a, b) => a.seq - b.seq);
  return boundTranscript(merged);
}

/** Recursively bound a JSON value (depth, array/keys, string lengths). */
export function boundJson(value: Json, depth = 0): Json {
  if (value === null || typeof value === 'boolean' || typeof value === 'number') {
    return value;
  }
  if (typeof value === 'string') {
    return value.length > MAX_SUMMARY_STRING ? `${safeSlice(value, MAX_SUMMARY_STRING)}…` : value;
  }
  if (depth >= 6) {
    return '[…]';
  }
  if (Array.isArray(value)) {
    return value.slice(0, MAX_SUMMARY_ARRAY).map((entry) => boundJson(entry, depth + 1));
  }
  const out: Record<string, Json> = {};
  let keys = 0;
  for (const [key, entry] of Object.entries(value)) {
    if (keys >= MAX_SUMMARY_KEYS) {
      out['…'] = `${Object.keys(value).length - keys} more`;
      break;
    }
    out[key] = boundJson(entry as Json, depth + 1);
    keys += 1;
  }
  return out;
}

function boundedJsonText(value: Json): string {
  try {
    const encoded = JSON.stringify(boundJson(value));
    if (encoded === undefined) {
      return 'null';
    }
    return encoded.length > MAX_SUMMARY_JSON_CHARS
      ? `${safeSlice(encoded, MAX_SUMMARY_JSON_CHARS)}…`
      : encoded;
  } catch {
    return '[unserializable]';
  }
}

/** One capability entry as a bounded, renderable string. */
function capabilityOf(value: Json): string {
  if (typeof value === 'string') {
    return value.length > MAX_SUMMARY_STRING ? `${safeSlice(value, MAX_SUMMARY_STRING)}…` : value;
  }
  return boundedJsonText(value);
}

/** Blockers carried by an agent entry, from the additive fields. */
function agentBlockers(agent: Record<string, Json>): string[] {
  const out: string[] = [];
  const push = (value: Json | undefined): void => {
    if (typeof value === 'string' && value.trim().length > 0) {
      out.push(safeSlice(value.trim(), MAX_SUMMARY_STRING));
    }
  };
  const pushObject = (value: Json | undefined): void => {
    if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
      const record = value as Record<string, Json>;
      push(record.reason);
      push(record.detail);
      push(record.message);
      push(record.summary);
      const kind = typeof record.kind === 'string' ? record.kind : null;
      const resolution = typeof record.resolution === 'string' ? record.resolution : null;
      if (kind !== null && out.length > 0 && resolution !== null) {
        out[out.length - 1] = `${kind}: ${out[out.length - 1]} (${resolution})`;
      }
    }
  };
  pushObject(agent.blocker);
  push(agent.blocker);
  const blockers = agent.blockers;
  if (Array.isArray(blockers)) {
    for (const entry of blockers.slice(0, 16)) {
      if (typeof entry === 'string') {
        push(entry);
      } else {
        pushObject(entry);
      }
    }
  } else {
    pushObject(blockers);
  }
  const progress = agent.progress;
  if (typeof progress === 'object' && progress !== null && !Array.isArray(progress)) {
    const record = progress as Record<string, Json>;
    const progressBlockers = record.blockers ?? record.blocked_on ?? record.blocker;
    if (Array.isArray(progressBlockers)) {
      for (const entry of progressBlockers.slice(0, 16)) {
        if (typeof entry === 'string') {
          push(entry);
        } else {
          pushObject(entry);
        }
      }
    } else {
      pushObject(progressBlockers);
      push(progressBlockers);
    }
  }
  return out.slice(0, 16);
}

/**
 * Fold one native agent frame into the persistent per-ChildId presence map:
 * existing presences keep their identity (deterministic avatars), current
 * frames update state, and transiently missing children are retained.
 */
export function nextPixelPresence(
  previous: ReadonlyMap<string, PixelPresence>,
  agents: readonly Json[],
): Map<string, PixelPresence> {
  const frame: Array<{ agentId: string; state: string }> = [];
  for (const raw of agents) {
    const agent = asObject(raw);
    if (!agent) {
      continue;
    }
    const agentId = asString(agent.agent_id);
    if (agentId) {
      frame.push({ agentId, state: asString(agent.state) ?? 'unknown' });
    }
  }
  return foldPixelPresence(previous, frame);
}

/**
 * Human-readable summary of one durable agent listing entry, with every
 * native child field surfaced (bounded) plus catalog-derived model
 * metadata and the deterministic pixel presence. `presence` (when given)
 * is the persistent per-ChildId map from previous frames.
 */
export function summarizeAgents(
  agents: readonly Json[],
  catalog: readonly AgentModelInfo[] = [],
  presence?: ReadonlyMap<string, PixelPresence>,
): AgentSummary[] {
  const folded = presence !== undefined ? nextPixelPresence(presence, agents) : null;
  // ONE catalog index (first entry wins, exactly the historical `find`):
  // the per-agent scan made a 200-agent listing O(agents · catalog).
  const catalogByProvider = new Map<string, Map<string, AgentModelInfo>>();
  for (const entry of catalog) {
    let byModel = catalogByProvider.get(entry.provider);
    if (byModel === undefined) {
      byModel = new Map();
      catalogByProvider.set(entry.provider, byModel);
    }
    if (!byModel.has(entry.model)) byModel.set(entry.model, entry);
  }
  const out: AgentSummary[] = [];
  for (const raw of agents) {
    const agent = asObject(raw);
    if (!agent) {
      continue;
    }
    const agentId = asString(agent.agent_id);
    if (!agentId) {
      continue;
    }
    const state = asString(agent.state) ?? 'unknown';
    const model = asString(agent.model);
    // The child session's durable provider rides the wire; the
    // (provider, model) pair is the ONLY safe catalog join key because two
    // providers may expose the same model id with different capabilities.
    // A provider-less entry never guesses by model alone.
    const provider = asString(agent.provider);
    const info =
      provider !== null && model !== null
        ? catalogByProvider.get(provider)?.get(model)
        : undefined;
    const itemIdsRaw = Array.isArray(agent.item_ids) ? (agent.item_ids as Json[]) : [];
    const capabilitiesRaw = Array.isArray(agent.capabilities) ? (agent.capabilities as Json[]) : [];
    const progress = agent.progress ?? null;
    const result = agent.result ?? null;
    out.push({
      agentId,
      kind: asString(agent.kind) ?? 'child',
      runId: asString(agent.run_id) ?? '',
      state,
      goal: safeSlice(asString(agent.goal) ?? '', MAX_ENTRY_CHARS),
      model,
      provider: provider ?? null,
      reasoning: info?.reasoning ?? null,
      thinking: info?.thinking ?? null,
      itemId: asString(agent.item_id),
      itemKind: asString(agent.item_kind),
      itemIds: itemIdsRaw
        .filter((entry): entry is string => typeof entry === 'string')
        .slice(0, MAX_SUMMARY_ARRAY),
      sessionId: asInt(agent.session_id),
      worktreeId: asInt(agent.worktree_id),
      ownership: asString(agent.ownership) ?? 'unknown',
      capabilities: capabilitiesRaw.slice(0, MAX_SUMMARY_ARRAY).map(capabilityOf),
      progress: boundJson(progress),
      result: boundJson(result),
      budget: asInt(agent.budget),
      blockers: agentBlockers(agent),
      presentation: agent.presentation === 'background' ? 'background' : 'foreground',
      pixel: folded?.get(agentId) ?? pixelPresence(agentId, state),
    });
  }
  return out;
}

