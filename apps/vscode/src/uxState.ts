// The shared composer/run UX state model for both Faktor hosts.
//
// Product boundary: the model translates served machine conditions into a
// SMALL vocabulary of user-facing states. Raw enum names, provider error
// text and identifiers are never part of this projection: they live ONLY in
// the Diagnostics surface, which reads the served snapshot directly. The
// same enum names are mirrored in
// `apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/UxStateModel.kt`.
//
// Everything here is pure and dependency-free so the webview contract, the
// extension projection and the headless tests share one source of truth.

export type ComposerIntent = 'ask' | 'task';

export const DEFAULT_COMPOSER_INTENT: ComposerIntent = 'ask';

/** Normalise a submitted composer intent. Anything but `task` is Ask. */
export function composerIntentOf(raw: unknown): ComposerIntent {
  return raw === 'task' ? 'task' : 'ask';
}

export type TaskPresentation =
  | 'Idle'
  | 'Starting'
  | 'Planning'
  | 'Working'
  | 'AwaitingUser'
  | 'Checking'
  | 'Done'
  | 'Paused'
  | 'Failed';

export type AttentionKind =
  | 'Approval'
  | 'FailedCheck'
  | 'MissingCredential'
  | 'AmbiguousRequirement'
  | 'RepositoryLimitation';

export type UxActionKey =
  | 'openProviderSettings'
  | 'retryProvider'
  | 'reconnect'
  | 'resumeRun'
  | 'retryStart'
  | 'reply'
  | 'openChecks'
  | 'openDiagnostics';

export interface UxAction {
  readonly key: UxActionKey;
  readonly label: string;
}

export interface AttentionItem {
  readonly kind: AttentionKind;
  readonly label: string;
  readonly detail: string | null;
  readonly count: number;
  readonly action: UxAction | null;
}

export interface UxSummary {
  readonly doing: string | null;
  readonly wrong: string | null;
  readonly changed: string | null;
  readonly checks: string | null;
  readonly cost: string | null;
}

export interface UxProjection {
  readonly intent: ComposerIntent;
  readonly presentation: TaskPresentation;
  readonly presentationLabel: string;
  readonly presentationDetail: string;
  readonly presentationAction: UxAction | null;
  /** Aggregated, at most three groups, most actionable/informative first. */
  readonly attention: readonly AttentionItem[];
  readonly summary: UxSummary;
}

export interface UxRunLike {
  readonly state: string;
}

export interface UxTaskLike {
  readonly goal?: string | null;
  readonly state?: string | null;
  readonly phase?: string | null;
  readonly blockers?: readonly string[];
  readonly acceptanceCriteria?: readonly string[];
  readonly open?: readonly string[];
  readonly budget?: {
    readonly spentTokens?: number | null;
    readonly maxTokens?: number | null;
    readonly spentCostMicro?: string | null;
    readonly maxCostMicro?: string | null;
  } | null;
}

export interface UxChecksLike {
  readonly passed: number;
  readonly total: number;
  readonly failed: number;
  readonly requiredPassed?: number;
  readonly requiredTotal?: number;
  readonly requiredFailed?: number;
}

export interface UxProofLike {
  readonly state?: string | null;
  readonly checks?: UxChecksLike | null;
  readonly cost?: { readonly spentMicro?: string | null } | null;
}

export interface UxVerificationLike {
  readonly owed?: unknown;
  readonly failedChecks?: unknown;
  readonly status?: string | null;
  readonly checksFailed?: number | null;
  readonly checks?: UxChecksLike | null;
  readonly criteriaPassed?: number | null;
  readonly criteriaTotal?: number | null;
}

export interface UxChangesLike {
  readonly filesChanged?: readonly unknown[];
  readonly rows?: readonly unknown[];
  readonly available?: boolean;
  readonly reason?: string | null;
  readonly unavailable?: string | null;
}

export interface UxInput {
  readonly daemon?: string | null;
  readonly machineState?: string | null;
  readonly machineLabel?: string | null;
  readonly busy?: boolean;
  readonly activeRunId?: string | null;
  readonly runs?: readonly UxRunLike[];
  readonly proofTaskState?: string | null;
  readonly task?: UxTaskLike | null;
  readonly verification?: UxVerificationLike | null;
  readonly proof?: UxProofLike | null;
  readonly cockpit?: {
    readonly verification?: UxVerificationLike | null;
    readonly proof?: UxProofLike | null;
  } | null;
  readonly spend?: { readonly spentCostMicro?: string | null; readonly spentTokens?: number | null } | null;
  readonly changes?: UxChangesLike | null;
  readonly permissions?: readonly unknown[];
  readonly streamStatus?: string | null;
  readonly lastError?: string | null;
  readonly errorKind?: string | null;
}

const MAX_ATTENTION_GROUPS = 3;
const MAX_LABEL_CHARS = 72;
const MAX_DETAIL_CHARS = 220;

function text(value: unknown): string {
  return typeof value === 'string' ? value.trim() : '';
}

function lower(value: unknown): string {
  return text(value).toLowerCase();
}

function bound(value: string, max: number): string {
  if (value.length <= max) return value;
  return `${value.slice(0, Math.max(0, max - 1))}\u2026`;
}

function countLabel(count: number, singular: string, plural: string): string {
  return count === 1 ? `1 ${singular}` : `${count} ${plural}`;
}

/** Exact micro-USD -> a human dollar amount (`$5.00`, `$1.2346`). */
export function humanUsd(micro: string | null | undefined): string | null {
  const raw = text(micro);
  if (!/^[0-9]+$/.test(raw)) return null;
  const value = BigInt(raw);
  if (value === 0n) return '$0.00';
  if (value < 10_000n) return 'under $0.01';
  const whole = value / 1_000_000n;
  const remainder = value % 1_000_000n;
  let cents = (remainder + 5_000n) / 10_000n;
  let dollars = whole;
  if (cents >= 100n) {
    dollars += 1n;
    cents -= 100n;
  }
  if (cents === 0n) return `$${dollars}.00`;
  return `$${dollars}.${cents.toString().padStart(2, '0')}`;
}

export function humanCount(value: number): string {
  if (!Number.isFinite(value)) return '\u2014';
  return Math.trunc(value).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ',');
}

interface Cause {
  readonly kind: AttentionKind | null;
  readonly presentation: TaskPresentation;
  readonly label: string;
  readonly detail: string;
  readonly action: UxAction | null;
}

const OPEN_PROVIDER: UxAction = { key: 'openProviderSettings', label: 'Open provider settings' };
const OPEN_CHECKS: UxAction = { key: 'openChecks', label: 'View checks' };
const RESUME: UxAction = { key: 'resumeRun', label: 'Resume' };
const RETRY_START: UxAction = { key: 'retryStart', label: 'Retry' };
const REPLY: UxAction = { key: 'reply', label: 'Reply' };
const DIAGNOSTICS: UxAction = { key: 'openDiagnostics', label: 'Diagnostics' };
const RECONNECT: UxAction = { key: 'reconnect', label: 'Reconnect' };

const CREDENTIAL_PATTERN =
  /(credential|api[ _-]?key|unauthori[sz]ed|401|403|auth(?:entication|orization)? (?:failed|required)|invalid key|missing key|token (?:expired|invalid)|no provider)/i;
const TRANSIENT_PATTERN =
  /(rate.?limit|429|timeout|timed out|temporar|transient|overload|unavailable|5\d\d|connection reset|econnreset|try again|retry)/i;
const NETWORK_PATTERN = /(network|offline|dns|socket|fetch failed|connection (?:lost|refused|interrupted))/i;
const AMBIGUOUS_PATTERN =
  /(ambiguous|clarif|question|needs? (?:your )?input|needs? user|insufficient (?:detail|information)|which (?:file|approach|one)|requirement)/i;
const REPOSITORY_PATTERN =
  /(repository|worktree|read.?only|unsupported|not supported|too large|path traversal|outside the workspace|limitation)/i;

function failureCause(input: UxInput, checksFailed: boolean): Cause | null {
  const machine = lower(input.machineState);
  const run = lower(input.runs?.[0]?.state);
  const proof = lower(input.proofTaskState);
  const stream = lower(input.streamStatus);
  const errorKind = lower(input.errorKind);
  const raw = `${input.lastError ?? ''} ${input.machineLabel ?? ''} ${machine} ${run} ${proof}`;
  const active = input.activeRunId != null || input.busy === true;
  const failed =
    machine === 'failed' ||
    machine === 'failed_recoverable' ||
    run === 'failed' ||
    proof === 'failed' ||
    errorKind === 'run_failure' ||
    errorKind === 'failed';

  if (CREDENTIAL_PATTERN.test(raw)) {
    return {
      kind: 'MissingCredential',
      presentation: 'Failed',
      label: 'Provider setup needed',
      detail: 'The model provider rejected its credential. Open provider settings, then resume the run.',
      action: OPEN_PROVIDER,
    };
  }
  if (lower(input.daemon) === 'stopped' || lower(input.daemon) === 'error') {
    return {
      kind: null,
      presentation: 'Paused',
      label: 'Reconnecting',
      detail: 'The service connection was lost. Faktor reconnects automatically; your work is saved.',
      action: RECONNECT,
    };
  }
  if (stream === 'protocol_blocked' || stream === 'blocked') {
    return {
      kind: null,
      presentation: 'Paused',
      label: 'Connection interrupted',
      detail: 'Live updates are paused. Your work is saved; Faktor reconnects from the last good position.',
      action: RECONNECT,
    };
  }
  if (run === 'paused' || machine === 'paused') {
    return {
      kind: null,
      presentation: 'Paused',
      label: 'Paused',
      detail: 'Your task is saved. Resume it when you are ready; nothing needs to be sent again.',
      action: RESUME,
    };
  }
  if (TRANSIENT_PATTERN.test(raw) && failed) {
    return {
      kind: null,
      presentation: 'Working',
      label: 'Retrying automatically',
      detail: 'The provider was briefly unavailable. Faktor is retrying automatically; nothing was lost.',
      action: null,
    };
  }
  if (NETWORK_PATTERN.test(raw)) {
    return {
      kind: null,
      presentation: 'Paused',
      label: 'Connection interrupted',
      detail: 'The connection was interrupted. Your work is saved; Faktor reconnects automatically.',
      action: RECONNECT,
    };
  }
  if (checksFailed) {
    return {
      kind: 'FailedCheck',
      presentation: 'Failed',
      label: 'Checks found a problem',
      detail: 'A required check did not pass. Review the failing check before continuing.',
      action: OPEN_CHECKS,
    };
  }
  if (lower(input.machineState) === 'needs_user_input' || (input.permissions?.length ?? 0) > 0) {
    return {
      kind: input.permissions && input.permissions.length > 0 ? 'Approval' : 'AmbiguousRequirement',
      presentation: 'AwaitingUser',
      label: 'Needs your input',
      detail: 'The run is waiting for a decision or clarification. Reply in the composer.',
      action: REPLY,
    };
  }
  if (!active && failed) {
    return {
      kind: null,
      presentation: 'Failed',
      label: "Couldn't start",
      detail: 'The task was not started. Your draft was kept; Retry uses the same request.',
      action: RETRY_START,
    };
  }
  if (failed) {
    return {
      kind: null,
      presentation: 'Failed',
      label: "Couldn't continue",
      detail: 'The run stopped before it could finish. Open Diagnostics for the cause.',
      action: DIAGNOSTICS,
    };
  }
  return null;
}

/** The base presentation when no cause-specific override applies. */
function basePresentation(input: UxInput): TaskPresentation {
  if (lower(input.machineState) === 'needs_user_input') return 'AwaitingUser';
  if ((input.permissions?.length ?? 0) > 0) return 'AwaitingUser';
  const run = lower(input.runs?.[0]?.state);
  const proof = lower(input.proofTaskState);
  const task = lower(input.task?.state);
  const phase = lower(input.task?.phase);
  const active = input.busy === true || input.activeRunId != null;
  if (proof === 'verifying' || proof === 'needs_verification') return 'Checking';
  if (run === 'verifying' || run === 'needs_verification' || task === 'verifying') return 'Checking';
  if (run === 'paused' || task === 'paused') return 'Paused';
  if (run === 'done' || run === 'complete' || run === 'completed' || task === 'done') return 'Done';
  if (run === 'failed' || task === 'failed') return 'Failed';
  if (run === 'starting' || run === 'admitted' || run === 'pending') return 'Starting';
  if (run === 'planning' || phase === 'planning' || phase === 'plan') return 'Planning';
  if (run === 'running' || run === 'working' || run === 'in_progress' || active) return 'Working';
  return 'Idle';
}

const PRESENTATION_COPY: Record<TaskPresentation, { label: string; detail: string; action: UxAction | null }> = {
  Idle: {
    label: 'Ready',
    detail: 'Ask a question, or switch to Task for a full run.',
    action: null,
  },
  Starting: {
    label: 'Starting',
    detail: 'The request is being admitted once.',
    action: null,
  },
  Planning: {
    label: 'Planning',
    detail: 'Faktor is breaking the work into steps.',
    action: null,
  },
  Working: {
    label: 'Working',
    detail: 'Faktor is working in an isolated copy; your files are untouched until checks pass.',
    action: null,
  },
  AwaitingUser: {
    label: 'Needs your input',
    detail: 'The run is waiting for a decision or clarification.',
    action: REPLY,
  },
  Checking: {
    label: 'Checking the changes',
    detail: 'Required checks are still running against the candidate.',
    action: OPEN_CHECKS,
  },
  Done: {
    label: 'Done',
    detail: 'The run finished.',
    action: null,
  },
  Paused: {
    label: 'Paused',
    detail: 'The run is saved and can continue from its durable checkpoint.',
    action: RESUME,
  },
  Failed: {
    label: "Couldn't continue",
    detail: 'The run stopped before it could finish.',
    action: DIAGNOSTICS,
  },
};

function proofOf(input: UxInput): UxProofLike | null {
  return input.cockpit?.proof ?? input.proof ?? null;
}

function verificationOf(input: UxInput): UxVerificationLike | null {
  return input.cockpit?.verification ?? input.verification ?? null;
}

function checksOf(input: UxInput): UxChecksLike | null {
  const proofChecks = proofOf(input)?.checks;
  if (proofChecks && Number.isFinite(proofChecks.total) && proofChecks.total > 0) return proofChecks;
  const verificationChecks = verificationOf(input)?.checks;
  if (verificationChecks && Number.isFinite(verificationChecks.total) && verificationChecks.total > 0) {
    return verificationChecks;
  }
  return null;
}

function checksFailedOf(input: UxInput): boolean {
  const checks = checksOf(input);
  if (checks !== null && (checks.failed > 0 || (checks.requiredFailed ?? 0) > 0)) return true;
  if ((verificationOf(input)?.checksFailed ?? 0) > 0) return true;
  return lower(verificationOf(input)?.status) === 'failed';
}

function attentionItem(
  kind: AttentionKind,
  label: string,
  detail: string | null,
  count: number,
  action: UxAction | null,
): AttentionItem {
  return {
    kind,
    label: bound(label, MAX_LABEL_CHARS),
    detail: detail === null ? null : bound(detail, MAX_DETAIL_CHARS),
    count: Math.max(1, count),
    action,
  };
}

/**
 * Aggregate served blockers into at most three user-facing groups. A group
 * counts its members; one row per group, never a wall of identical rows.
 * Absent conditions produce no group at all.
 */
export function aggregateAttention(input: UxInput): readonly AttentionItem[] {
  const checksFailed = checksFailedOf(input);
  const cause = failureCause(input, checksFailed);
  const groups: AttentionItem[] = [];
  const approvalCount = input.permissions?.length ?? 0;
  if (approvalCount > 0) {
    groups.push(
      attentionItem(
        'Approval',
        countLabel(approvalCount, 'approval waiting', 'approvals waiting'),
        'Review the requested action before the run continues.',
        approvalCount,
        REPLY,
      ),
    );
  }
  if (cause?.kind === 'MissingCredential') {
    groups.push(attentionItem('MissingCredential', 'Provider connection needs attention', cause.detail, 1, OPEN_PROVIDER));
  }
  if (checksFailed) {
    const checks = checksOf(input);
    // A failed verification with ZERO findings (the run was interrupted
    // before any check executed, or only required checks are unproven) must
    // still surface as NOT passed — but it must never invent a
    // "0 checks found a problem" finding: the verdict itself becomes the
    // row's label and the count stays at least one.
    const failedCount = checks?.failed ?? verificationOf(input)?.checksFailed ?? 0;
    const requiredCount = checks?.requiredFailed ?? 0;
    const statusFailed = verificationOf(input)?.status === 'failed';
    // Verdict-first when no check FAILED: an interrupted run leaves required
    // checks UNPROVEN (failed=0, requiredFailed>0) — never label that "0
    // checks found a problem", and never invent failing checks that did not
    // fail; the verdict is the row and the detail names the unproven work.
    groups.push(
      attentionItem(
        'FailedCheck',
        failedCount > 0
          ? countLabel(failedCount, 'check found a problem', 'checks found a problem')
          : 'Verification did not pass',
        failedCount > 0
          ? statusFailed
            ? 'Verification did not pass.'
            : null
          : requiredCount > 0
            ? `${countLabel(requiredCount, 'required check did not complete', 'required checks did not complete')}.`
            : statusFailed
              ? 'The verification ended without a passing result.'
              : null,
        Math.max(1, failedCount > 0 ? failedCount : requiredCount),
        OPEN_CHECKS,
      ),
    );
  }
  const blockers = input.task?.blockers ?? [];
  const ambiguous = blockers.filter((blocker) => AMBIGUOUS_PATTERN.test(blocker));
  if (ambiguous.length > 0) {
    groups.push(
      attentionItem(
        'AmbiguousRequirement',
        countLabel(ambiguous.length, 'question needs your answer', 'questions need your answer'),
        ambiguous[0] ?? null,
        ambiguous.length,
        REPLY,
      ),
    );
  }
  if (cause?.kind === 'AmbiguousRequirement' && ambiguous.length === 0) {
    groups.push(attentionItem('AmbiguousRequirement', 'Needs your input', cause.detail, 1, REPLY));
  }
  const repository = blockers.filter(
    (blocker) => REPOSITORY_PATTERN.test(blocker) && !AMBIGUOUS_PATTERN.test(blocker),
  );
  if (repository.length > 0) {
    groups.push(
      attentionItem(
        'RepositoryLimitation',
        countLabel(repository.length, 'repository limitation', 'repository limitations'),
        repository[0] ?? null,
        repository.length,
        DIAGNOSTICS,
      ),
    );
  }
  // Deduplicate by kind, folding counts (a failure cause and a served row
  // can describe the same condition). Keep the first action/detail.
  const order: AttentionKind[] = [
    'Approval',
    'MissingCredential',
    'FailedCheck',
    'AmbiguousRequirement',
    'RepositoryLimitation',
  ];
  const folded = new Map<AttentionKind, AttentionItem>();
  for (const item of groups) {
    const existing = folded.get(item.kind);
    if (existing === undefined) {
      folded.set(item.kind, item);
    } else {
      folded.set(item.kind, {
        ...existing,
        count: existing.count + item.count,
        label: bound(
          existing.count + item.count === 1
            ? existing.label
            : `${existing.label} + ${item.count}`,
          MAX_LABEL_CHARS,
        ),
      });
    }
  }
  return order
    .map((kind) => folded.get(kind))
    .filter((item): item is AttentionItem => item !== undefined)
    .slice(0, MAX_ATTENTION_GROUPS);
}

function summaryOf(input: UxInput, presentation: TaskPresentation): UxSummary {
  const task = input.task ?? null;
  const checks = checksOf(input);
  const checksFailed = checksFailedOf(input);
  const attention = aggregateAttention(input);
  const doing = task?.goal
    ? bound(text(task.goal), MAX_LABEL_CHARS)
    : presentation === 'Idle'
      ? null
      : PRESENTATION_COPY[presentation].label;
  const wrong =
    attention.length === 0
      ? null
      : attention.length === 1
        ? attention[0]!.label
        : `${attention[0]!.label} (+${attention.length - 1} more)`;
  let changed: string | null = null;
  const unavailable =
    input.changes?.unavailable ?? (input.changes?.available === false ? input.changes?.reason ?? 'unavailable' : null);
  if (unavailable) {
    changed = 'Changes unavailable';
  } else {
    const files = input.changes?.filesChanged?.length ?? input.changes?.rows?.length ?? 0;
    if (files > 0) changed = countLabel(files, 'file changed', 'files changed');
  }
  let checksText: string | null = null;
  if (checks !== null) {
    const passed = checks.requiredPassed ?? checks.passed;
    const total = checks.requiredTotal ?? checks.total;
    if (checksFailed) {
      checksText = `${humanCount(passed)} of ${humanCount(total)} checks passed`;
    } else if (passed >= total) {
      checksText = `${humanCount(total)} of ${humanCount(total)} checks passed`;
    } else {
      checksText = `${humanCount(passed)} of ${humanCount(total)} checks passed`;
    }
  } else if (task !== null) {
    const toRun = Math.max(
      task.acceptanceCriteria?.length ?? 0,
      task.open?.length ?? 0,
    );
    if (toRun > 0) checksText = `Checks to run \u00b7 ${humanCount(toRun)}`;
  }
  const costMicro =
    proofOf(input)?.cost?.spentMicro ??
    input.task?.budget?.spentCostMicro ??
    input.spend?.spentCostMicro ??
    null;
  const usd = humanUsd(costMicro);
  const tokens =
    input.task?.budget?.spentTokens ?? input.spend?.spentTokens ?? null;
  const cost =
    usd !== null
      ? usd
      : typeof tokens === 'number' && Number.isFinite(tokens)
        ? `${humanCount(tokens)} tokens`
        : null;
  return {
    doing,
    wrong,
    changed,
    checks: checksText,
    cost,
  };
}

/** Project one served snapshot/condition set into the shared UX model. */
export function projectUx(input: UxInput, intent: ComposerIntent = DEFAULT_COMPOSER_INTENT): UxProjection {
  const checksFailed = checksFailedOf(input);
  const cause = failureCause(input, checksFailed);
  const presentation = cause?.presentation ?? basePresentation(input);
  const base = PRESENTATION_COPY[presentation];
  return {
    intent,
    presentation,
    presentationLabel: cause?.label ?? base.label,
    presentationDetail: cause?.detail ?? base.detail,
    presentationAction: cause?.action ?? base.action,
    attention: aggregateAttention(input),
    summary: summaryOf(input, presentation),
  };
}

/** The empty projection used before the first served snapshot. */
export function emptyUx(): UxProjection {
  return projectUx({}, DEFAULT_COMPOSER_INTENT);
}

/**
 * Four invariants the host and the tests pin:
 *   - Ask never carries a task contract;
 *   - a normal refresh never reads billing/account;
 *   - attention is capped at three groups;
 *   - every served failure maps to exactly one user label.
 * They are constants here so a mutation of one is observable in the tests.
 */
export const UX_INVARIANTS = {
  askCarriesContract: false,
  refreshReadsAccount: false,
  maxAttentionGroups: MAX_ATTENTION_GROUPS,
  pausedOffersResend: false,
} as const;
