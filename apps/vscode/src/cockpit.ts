// The Task cockpit: ONE persistent panel assembled from the native state
// the extension already holds — acceptance criteria, plan/DAG steps,
// children, current phase, blockers (additive fields when the daemon
// serves them), verification status, evidence refs (retrievable) and
// durable spend. Pure and dependency-free so scripts/selftest.mjs renders
// every section from a mock native payload.
//
// The builder never invents data: a missing field yields an explicitly
// empty section (`present: false`, one "none" line), never a fabricated
// value. Every string is bounded; evidence ids parse only from explicit
// `evidence:<n>` refs.

import type { AgentSummary, TaskSummary, UsageSummary, VerificationSummary } from './state';
import type {
  NativeBillingUsage,
  NativeEntitlementSnapshot,
  NativeIdentity,
} from './nativeClient';
import {
  isMicroLimitName,
  microBalance,
  microToString,
  microUsdText,
  type MicroMoney,
} from './money.ts';

export interface CockpitEvidenceRef {
  readonly id: number | null;
  readonly label: string;
}

// ------------------------------------------------ acceptance-criterion proof
//
// The proof view of ONE acceptance criterion. Everything here is projected
// from the native verification payload; a member the payload does not serve
// is represented as `unavailable` with the precise reason — never guessed
// and never as a pass. The `binding` kind is `daemon` when the record row
// carries an explicit binding, `derived` when it could be read from the
// criterion's typed evidence refs, and `unavailable` otherwise.

/** The typed criterion-binding kinds (the proof system's own vocabulary). */
export type CriterionBindingView =
  | 'required_check'
  | 'integration_coverage'
  | 'file_state'
  | 'evidence'
  | 'independent_review'
  | 'aggregate_goal'
  | 'unavailable';

/** Where the binding kind came from. */
export type CriterionBindingSource = 'daemon' | 'derived' | 'unavailable';

/** The criterion requirement (`preferred` is the advisory level). */
export type CriterionRequirementView = 'required' | 'preferred' | 'unavailable';

/** The criterion origin (who derived the criterion). */
export type CriterionOriginView =
  | 'user'
  | 'project_policy'
  | 'verification_policy'
  | 'semantic_provider'
  | 'unavailable';

/** A criterion's verdict. Unavailable NEVER renders as pass. */
export type CriterionVerdictView = 'pass' | 'fail' | 'unavailable';

/** Where the verdict came from. */
export type CriterionVerdictSource = 'daemon' | 'recorded' | 'unavailable';

/** The snapshot actually proven by one verification record. */
export interface CockpitProofSnapshotView {
  readonly candidate: string | null;
  readonly verified: string | null;
  readonly basedOn: string | null;
  readonly landed: string | null;
  readonly sourceCount: number | null;
  readonly runId: string | null;
  readonly taskRevision: string | null;
  readonly baseManifestHash: string | null;
  readonly candidateManifestHash: string | null;
  readonly sourcesDigest: string | null;
  readonly changedFilesDigest: string | null;
  readonly accountingSnapshotDigest: string | null;
  readonly sourceDiffEvidence: string | null;
  readonly riskReportEvidence: string | null;
  /** The published commit OID when the daemon serves one. */
  readonly publishedCommit: string | null;
  /** The remote PR head when the daemon serves one. */
  readonly remotePrHead: string | null;
}

/** The verification timestamps of the record that judged the criterion. */
export interface CockpitProofTimestamp {
  readonly startedMs: number | null;
  readonly completedMs: number | null;
}

/** One acceptance-criterion proof row. */
export interface CockpitCriterionProof {
  readonly criterionKey: string;
  readonly requirement: CriterionRequirementView;
  readonly requirementReason: string | null;
  readonly origin: CriterionOriginView;
  readonly originReason: string | null;
  readonly binding: CriterionBindingView;
  readonly bindingSource: CriterionBindingSource;
  /** The exact check/evidence/reviewer/file/work-item reference. */
  readonly bindingReference: string | null;
  readonly bindingDetail: string | null;
  readonly verdict: CriterionVerdictView;
  readonly verdictSource: CriterionVerdictSource;
  readonly verdictReason: string | null;
  readonly evidenceRefs: readonly CockpitEvidenceRef[];
  readonly snapshot: CockpitProofSnapshotView;
  readonly timestamp: CockpitProofTimestamp;
  readonly recordId: string | null;
  readonly recordStatus: string | null;
  /** Names of the proof members this payload genuinely does not serve. */
  readonly unavailable: readonly string[];
}

export interface CockpitStepView {
  readonly id: string;
  readonly summary: string;
  readonly status: string;
  readonly dependsOn: readonly string[];
  readonly childIds: readonly string[];
}

export interface CockpitChildView {
  readonly childId: string;
  readonly state: string;
  readonly itemId: string | null;
  readonly itemKind: string | null;
  readonly worktreeId: number | null;
  readonly ownership: string;
  readonly model: string | null;
  readonly provider: string | null;
  readonly reasoning: boolean | null;
  readonly thinking: boolean | null;
  readonly budget: number | null;
  readonly progress: string | null;
  readonly result: string | null;
  readonly presentation: 'foreground' | 'background';
}

export interface CockpitVerificationView {
  readonly status: string;
  readonly criteriaPassed: number;
  readonly criteriaTotal: number;
  readonly checksFailed: number;
  readonly owed: number;
  readonly failedChecks: number;
  /**
   * True only when the served record proves the FULL criterion set with no
   * failed check: the summary may then render VERIFIED. A missing record is
   * never verified.
   */
  readonly verified: boolean;
  readonly reviewer: string | null;
  readonly tree: string | null;
  readonly publishedCommit: string | null;
  readonly remotePrHead: string | null;
}

export interface CockpitSpendView {
  readonly spentTokens: number | null;
  readonly maxTokens: number | null;
  /** Exact micro-USD as a canonical decimal string (never a rounded float). */
  readonly spentCostMicro: string;
  readonly maxCostMicro: string | null;
  readonly openReservedMicro: string;
}

/** One tournament candidate (mirrors the durable native candidate rows). */
export interface CockpitTournamentCandidateView {
  readonly childId: string;
  readonly state: string;
  readonly verification: number | null;
  readonly verificationPass: boolean | null;
  readonly reviewRank: string | null;
  readonly reviewer: string | null;
  /** Exact micro-USD as a canonical decimal string. */
  readonly costMicro: string;
  readonly wallMs: number;
  readonly winner: boolean;
}

/** The durable tournament as the cockpit renders it (never auto-integrates). */
export interface CockpitTournamentView {
  readonly id: string;
  readonly state: string;
  /** The engine can still act: open | deciding. */
  readonly open: boolean;
  /** Decide waits until every candidate settled (the engine's rule). */
  readonly canDecide: boolean;
  readonly winner: string | null;
  readonly criteria: readonly string[];
  readonly candidates: readonly CockpitTournamentCandidateView[];
}

export interface CockpitView {
  readonly goal: string;
  readonly state: string;
  readonly phase: string;
  readonly acceptanceCriteria: readonly string[];
  /** Per-criterion proof: why Faktor believes each criterion is complete. */
  readonly criteriaProof: readonly CockpitCriterionProof[];
  readonly steps: readonly CockpitStepView[];
  readonly children: readonly CockpitChildView[];
  readonly blockers: readonly string[];
  readonly verification: CockpitVerificationView;
  readonly evidence: readonly CockpitEvidenceRef[];
  readonly spend: CockpitSpendView | null;
  readonly tournament: CockpitTournamentView | null;
  /** The Task-mode completion contract + durable step statuses (null when
   * the run carries no contract). Provenance is explicit (`daemon`,
   * `derived` or `unavailable`) and never fabricated. */
  readonly completion: CockpitCompletionView | null;
  /** The top-level VERIFIED view from `GET /native/tasks/{id}/proof`.
   * `null` when the proof was never fetched; an explicit `unavailable`
   * state when the fetch/validation failed (never a stale VERIFIED). */
  readonly proof: CockpitProofView | null;
}

/** The completion-contract block as the cockpit renders it. */
export interface CockpitCompletionView {
  readonly includeCommit: boolean;
  readonly includePush: boolean;
  readonly includePr: boolean;
  readonly steps: readonly {
    readonly step: string;
    readonly status: string;
    readonly detail: string | null;
  }[];
  readonly source: string;
  readonly reason: string | null;
}

/** One durable completion step of the proof drill-down. */
export interface CockpitProofStepView {
  readonly step: string;
  readonly status: string;
  readonly present: boolean;
  readonly detail: string | null;
  readonly snapshot: string | null;
  readonly atMs: number | null;
}

/**
 * The top-level VERIFIED view derived from `GET /native/tasks/{id}/proof`.
 * `state` is the daemon's own three-way verdict, never re-derived here: the
 * cockpit renders `VERIFIED` ONLY for `verified`. A failed read is an
 * explicit `unavailable` carrying the refusal text; an `unverified` /
 * `unavailable` payload can never be presented as verified.
 */
export interface CockpitProofView {
  readonly state: 'verified' | 'unverified' | 'unavailable';
  readonly reason: string | null;
  readonly criteria: {
    readonly passed: number;
    readonly total: number;
    readonly failed: number;
    readonly unavailable: number;
    readonly certifiesCompletion: boolean;
  };
  readonly checks: {
    readonly passed: number;
    readonly total: number;
    readonly requiredPassed: number;
    readonly requiredTotal: number;
  };
  readonly review: {
    readonly status: string | null;
    readonly reviewer: string | null;
    readonly independentVerdict: string;
  };
  readonly trees: {
    readonly verified: string | null;
    readonly landed: string | null;
    readonly landedEqualsVerified: boolean | null;
  };
  readonly publication: {
    readonly commitOid: string | null;
    readonly remoteHeadOid: string | null;
    readonly pullRequestId: string | null;
  };
  readonly cost: {
    readonly status: string;
    /** Exact micro-USD as canonical decimal strings (null = not served). */
    readonly spentMicro: string | null;
    readonly maxMicro: string | null;
    readonly reason: string | null;
  };
  readonly completion: {
    readonly gate: string;
    readonly gateReason: string | null;
    readonly steps: readonly CockpitProofStepView[];
  } | null;
  readonly unavailable: readonly {
    readonly component: string;
    readonly kind: string;
    readonly reason: string;
  }[];
}

/**
 * The minimal structural shape of the strict proof payload this module
 * consumes (see `nativeClient.ts`'s `NativeTaskProof` for the full contract;
 * both are structurally compatible).
 */
export interface CockpitNativeProof {
  readonly proofState: string;
  readonly proofStateReason: string | null;
  readonly criteria: {
    readonly passed: number;
    readonly total: number;
    readonly failed: number;
    readonly unavailable: number;
    readonly certifiesCompletion: boolean;
  };
  readonly checks: {
    readonly passed: number;
    readonly total: number;
    readonly requiredPassed: number;
    readonly requiredTotal: number;
  };
  readonly review: {
    readonly status: string | null;
    readonly reviewer: unknown;
    readonly independentVerdict: string;
  };
  readonly trees: {
    readonly verified: string | null;
    readonly landed: string | null;
    readonly landedEqualsVerified: boolean | null;
  };
  readonly publication: {
    readonly commitOid: string | null;
    readonly remoteHeadOid: string | null;
    readonly pullRequest: { readonly id: string } | null;
  };
  readonly cost: {
    readonly status: string;
    readonly spentCostMicro: MicroMoney | null;
    readonly maxCostMicro: MicroMoney | null;
    readonly reason: string | null;
  };
  readonly completion: {
    readonly gate: { readonly status: string; readonly reason: string | null };
    readonly steps: readonly {
      readonly step: string;
      readonly status: string;
      readonly present: boolean;
      readonly detail: string | null;
      readonly snapshot: string | null;
      readonly atMs: number | null;
    }[];
  };
  readonly unavailable: readonly {
    readonly component: string;
    readonly kind: string;
    readonly reason: string;
  }[];
}

/**
 * Project the strict proof payload into the cockpit's verified view.
 * `unavailableReason` (a fetch/validation failure) yields an explicit
 * `unavailable` view — the caller must pass it instead of a stale proof.
 */
export function proofViewOf(
  proof: CockpitNativeProof | null,
  unavailableReason: string | null = null,
): CockpitProofView | null {
  if (proof === null) {
    if (unavailableReason === null) {
      return null;
    }
    return {
      state: 'unavailable',
      reason: clamp(unavailableReason),
      criteria: { passed: 0, total: 0, failed: 0, unavailable: 0, certifiesCompletion: false },
      checks: { passed: 0, total: 0, requiredPassed: 0, requiredTotal: 0 },
      review: { status: null, reviewer: null, independentVerdict: 'none' },
      trees: { verified: null, landed: null, landedEqualsVerified: null },
      publication: { commitOid: null, remoteHeadOid: null, pullRequestId: null },
      cost: { status: 'unavailable', spentMicro: null, maxMicro: null, reason: 'proof unavailable' },
      completion: null,
      unavailable: [
        { component: 'proof', kind: 'unavailable', reason: clamp(unavailableReason) },
      ],
    };
  }
  const state =
    proof.proofState === 'verified'
      ? 'verified'
      : proof.proofState === 'unavailable'
        ? 'unavailable'
        : 'unverified';
  return {
    state,
    reason: proof.proofStateReason === null ? null : clamp(proof.proofStateReason),
    criteria: {
      passed: proof.criteria.passed,
      total: proof.criteria.total,
      failed: proof.criteria.failed,
      unavailable: proof.criteria.unavailable,
      certifiesCompletion: proof.criteria.certifiesCompletion,
    },
    checks: {
      passed: proof.checks.passed,
      total: proof.checks.total,
      requiredPassed: proof.checks.requiredPassed,
      requiredTotal: proof.checks.requiredTotal,
    },
    review: {
      status: proof.review.status,
      reviewer: reviewerTextOf(proof.review.reviewer),
      independentVerdict: proof.review.independentVerdict,
    },
    trees: {
      verified: proof.trees.verified,
      landed: proof.trees.landed,
      landedEqualsVerified: proof.trees.landedEqualsVerified,
    },
    publication: {
      commitOid: proof.publication.commitOid,
      remoteHeadOid: proof.publication.remoteHeadOid,
      pullRequestId: proof.publication.pullRequest?.id ?? null,
    },
    cost: {
      status: proof.cost.status,
      spentMicro: proof.cost.spentCostMicro === null ? null : microToString(proof.cost.spentCostMicro),
      maxMicro: proof.cost.maxCostMicro === null ? null : microToString(proof.cost.maxCostMicro),
      reason: proof.cost.reason === null ? null : clamp(proof.cost.reason),
    },
    completion: proof.completion
      ? {
          gate: proof.completion.gate.status,
          gateReason:
            proof.completion.gate.reason === null ? null : clamp(proof.completion.gate.reason),
          steps: proof.completion.steps.map((step) => ({
            step: step.step,
            status: step.status,
            present: step.present,
            detail: step.detail === null ? null : clamp(step.detail),
            snapshot: step.snapshot,
            atMs: step.atMs,
          })),
        }
      : null,
    unavailable: proof.unavailable.map((entry) => ({
      component: entry.component,
      kind: entry.kind,
      reason: clamp(entry.reason),
    })),
  };
}

/** One bounded line of the top-level verification-proof summary. */
export function proofSummaryLine(proof: CockpitProofView): string {
  const head =
    proof.state === 'verified'
      ? 'VERIFIED'
      : proof.state === 'unavailable'
        ? 'VERIFICATION UNAVAILABLE'
        : 'NOT VERIFIED';
  const bits = [
    `criteria ${proof.criteria.passed}/${proof.criteria.total} passed`,
    `checks ${proof.checks.passed}/${proof.checks.total} passed (required ${proof.checks.requiredPassed}/${proof.checks.requiredTotal})`,
    `review ${proof.review.status ?? 'none'}`,
    proof.trees.landedEqualsVerified === null
      ? 'verified==landed: unknown'
      : `verified==landed: ${proof.trees.landedEqualsVerified ? 'yes' : 'NO'}`,
    proof.publication.commitOid === null ? null : `commit ${shortRef(proof.publication.commitOid)}`,
    proof.publication.remoteHeadOid === null
      ? null
      : `remote head ${shortRef(proof.publication.remoteHeadOid)}`,
    proof.publication.pullRequestId === null ? null : `PR ${proof.publication.pullRequestId}`,
    proof.cost.status === 'known'
      ? `spend ${proof.cost.spentMicro ?? '0'}\u00b5$${
          proof.cost.maxMicro === null ? '' : ` of ${proof.cost.maxMicro}\u00b5$`
        }`
      : `spend unavailable${proof.cost.reason === null ? '' : ` (${proof.cost.reason})`}`,
  ].filter((bit): bit is string => bit !== null);
  return `${head} \u2014 ${bits.join(' \u00b7 ')}`;
}

function shortRef(value: string): string {
  return value.length > 12 ? `${value.slice(0, 12)}\u2026` : value;
}

/** One bounded line per durable completion step (proof drill-down). */
export function proofStepLines(proof: CockpitProofView): string[] {
  if (proof.completion === null) {
    return ['no completion contract recorded'];
  }
  const lines = proof.completion.steps.map(
    (step) =>
      `[${step.status}] ${step.step}${
        step.present ? '' : ' (no durable status row)'
      }${step.detail !== null && step.detail.length > 0 ? ` \u2014 ${step.detail}` : ''}`,
  );
  lines.push(
    `gate: ${proof.completion.gate}${
      proof.completion.gateReason === null ? '' : ` \u2014 ${proof.completion.gateReason}`
    }`,
  );
  return lines;
}

// ------------------------------------------------------- usage / credits panel
//
// The commercial-metering panel: current-period tokens and cost (managed vs
// BYOK), quota/limit progress with the EXACT limit name from the snapshot,
// subscription state (active/expired/canceled/grace/unavailable), the credit
// balance from the control plane, cursor pagination and the role-aware
// grant-credits affordance. Everything is projected from the served payload;
// a refusal is an explicit disabled/unavailable state carrying the refusal
// text — zeros are NEVER rendered in its place.

/** The exact limit names the daemon's plan config uses. */
export const USAGE_LIMIT_MAX_TOKENS = 'max_tokens_per_period';
export const USAGE_LIMIT_MANAGED_SPEND = 'max_managed_spend_micro_per_period';
export const USAGE_LIMIT_MIN_CREDIT = 'min_credit_balance_micro';
export const USAGE_LIMIT_ACTIVE_TASKS = 'max_active_tasks';
export const USAGE_LIMIT_CHILDREN_PER_TASK = 'max_children_per_task';
export const USAGE_LIMIT_ATTEMPTS_PER_TASK = 'max_provider_attempts_per_task';

/** Why the panel cannot show numbers: disabled locally, or a refused read. */
export type CockpitUsagePanelState = 'ok' | 'disabled' | 'unavailable';

/** The effective subscription state; `grace` = durable active row, inactive. */
export type CockpitSubscriptionState =
  | 'active'
  | 'expired'
  | 'canceled'
  | 'grace'
  | 'unavailable';

export interface CockpitUsageQuota {
  /** The EXACT limit name from the entitlement snapshot. */
  readonly limit: string;
  /** Exact limit value as a canonical decimal string (money or counter). */
  readonly value: string;
  /** The observed counter, or null when this snapshot serves none. */
  readonly observed: string | null;
  /** True only when the observed counter is known to breach the limit. */
  readonly exceeded: boolean | null;
  /** `ceiling` limits are exceeded at/above the value; `floor` below it. */
  readonly kind: 'ceiling' | 'floor';
  readonly reason: string | null;
}

export interface CockpitUsageBucketView {
  readonly inputTokens: number;
  readonly outputTokens: number;
  readonly cacheReadTokens: number;
  readonly cacheWriteTokens: number;
  readonly reasoningTokens: number;
  /** Exact micro-USD as canonical decimal strings (never a rounded float). */
  readonly providerCostMicro: string;
  readonly managedCostMicro: string;
  readonly byokCostMicro: string;
  readonly events: number;
  readonly correctedEvents: number;
}

export interface CockpitUsageTaskView {
  readonly taskId: number;
  readonly runId: string;
  readonly totals: CockpitUsageBucketView;
}

export interface CockpitUsagePeriodView extends CockpitUsageBucketView {
  readonly totalTokens: number;
  readonly tasks: readonly CockpitUsageTaskView[];
  /** The fold's own scan-bound cursor (informational; not the page cursor). */
  readonly scanCursor: string | null;
}

export interface CockpitUsageCreditsView {
  /** Exact micro-USD as canonical decimal strings (never a rounded float). */
  readonly grantedMicro: string;
  readonly consumedMicro: string;
  readonly refundedMicro: string;
  readonly heldMicro: string;
  readonly pendingConsumes: number;
  /** grants + refunds − effective debits, saturating (the server's rule). */
  readonly balanceMicro: string;
}

export interface CockpitUsagePageView {
  /** The cursor this page was read with (null = first page). */
  readonly cursor: string | null;
  readonly nextCursor: string | null;
  readonly itemCount: number;
  readonly hasPrev: boolean;
}

export interface CockpitUsagePanel {
  readonly state: CockpitUsagePanelState;
  /** The explicit refusal/disabled reason; null only in the `ok` state. */
  readonly reason: string | null;
  readonly organization: string | null;
  readonly planId: string | null;
  readonly planFound: boolean;
  readonly subscription: {
    readonly state: CockpitSubscriptionState;
    readonly status: string | null;
    readonly expiresMs: number | null;
    readonly active: boolean | null;
  };
  readonly period: CockpitUsagePeriodView | null;
  readonly credits: CockpitUsageCreditsView | null;
  readonly quotas: readonly CockpitUsageQuota[];
  readonly inFlight: readonly {
    readonly id: string;
    readonly kind: string;
    readonly reference: string;
    readonly startedMs: number;
    readonly endedMs: number | null;
  }[];
  readonly page: CockpitUsagePageView;
  readonly canGrantCredits: boolean;
  readonly grantDisabledReason: string | null;
  readonly actions: readonly CockpitAction[];
}

export interface CockpitUsageInput {
  readonly identity: NativeIdentity | null;
  readonly entitlements: NativeEntitlementSnapshot | null;
  readonly usage: NativeBillingUsage | null;
  /**
   * The explicit refusal of the last read (`NativeApiError` code + message,
   * or a validation failure). A `billing_disabled` refusal renders the
   * "billing disabled locally" state; anything else is unavailable.
   */
  readonly refusal: { readonly code: string; readonly reason: string } | null;
  readonly cursor: string | null;
  readonly hasPrev: boolean;
}

function usageBucketView(bucket: NativeBillingUsage['fold']['totals']): CockpitUsageBucketView {
  return {
    inputTokens: bucket.input_tokens,
    outputTokens: bucket.output_tokens,
    cacheReadTokens: bucket.cache_read_tokens,
    cacheWriteTokens: bucket.cache_write_tokens,
    reasoningTokens: bucket.reasoning_tokens,
    providerCostMicro: microToString(bucket.provider_cost_micro),
    managedCostMicro: microToString(bucket.managed_cost_micro),
    byokCostMicro: microToString(bucket.byok_cost_micro),
    events: bucket.events,
    correctedEvents: bucket.corrected_events,
  };
}

function creditBalanceView(
  credits: NativeBillingUsage['credits'],
): CockpitUsageCreditsView {
  const balanceMicro = microBalance(
    credits.granted_micro,
    credits.refunded_micro,
    credits.consumed_micro,
  );
  return {
    grantedMicro: microToString(credits.granted_micro),
    consumedMicro: microToString(credits.consumed_micro),
    refundedMicro: microToString(credits.refunded_micro),
    heldMicro: microToString(credits.held_micro),
    pendingConsumes: credits.pending_consumes,
    balanceMicro: microToString(balanceMicro),
  };
}

function subscriptionViewOf(
  snapshot: NativeEntitlementSnapshot | null,
): CockpitUsagePanel['subscription'] {
  if (snapshot === null) {
    return { state: 'unavailable', status: null, expiresMs: null, active: null };
  }
  const status = snapshot.subscription_status;
  const expiresMs = snapshot.subscription_expires_ms;
  const active = snapshot.subscription_active;
  if (active) {
    return { state: 'active', status, expiresMs, active };
  }
  if (status === 'expired' || status === 'canceled') {
    return { state: status, status, expiresMs, active };
  }
  if (status === 'active') {
    // The durable row says active but the effective snapshot is inactive:
    // the expiry has passed (the server derives, never sweeps). Honest
    // grace/lapse rendering — never reported as active.
    return { state: 'grace', status, expiresMs, active };
  }
  return { state: 'unavailable', status, expiresMs, active };
}

function quotaRowsOf(snapshot: NativeEntitlementSnapshot | null, panel: {
  readonly tokens: number | null;
  readonly managedMicro: MicroMoney | null;
  readonly balanceMicro: MicroMoney | null;
}): CockpitUsageQuota[] {
  if (snapshot === null) {
    return [];
  }
  const rows: CockpitUsageQuota[] = [];
  for (const [limit, value] of Object.entries(snapshot.limits).sort(([a], [b]) =>
    a.localeCompare(b),
  )) {
    let observed: MicroMoney | null = null;
    let reason: string | null = null;
    switch (limit) {
      case USAGE_LIMIT_MAX_TOKENS:
        observed = panel.tokens === null ? null : BigInt(panel.tokens);
        if (observed === null) {
          reason = 'the entitlement snapshot serves no token counter';
        }
        break;
      case USAGE_LIMIT_MANAGED_SPEND:
        observed = panel.managedMicro;
        if (observed === null) {
          reason = 'the entitlement snapshot serves no managed-spend counter';
        }
        break;
      case USAGE_LIMIT_MIN_CREDIT:
        observed = panel.balanceMicro;
        if (observed === null) {
          reason = 'the entitlement snapshot serves no credit balance';
        }
        break;
      default:
        reason = 'no observed counter is served for this limit';
        break;
    }
    const kind = limit === USAGE_LIMIT_MIN_CREDIT ? 'floor' : 'ceiling';
    // Exact comparison: money limits are micro-USD, other limits are plain
    // counters, but both are bigint so a u64 value never rounds.
    const exceeded =
      observed === null
        ? null
        : kind === 'floor'
          ? observed < value
          : observed >= value;
    rows.push({
      limit,
      value: microToString(value),
      observed: observed === null ? null : microToString(observed),
      exceeded,
      kind,
      reason,
    });
  }
  return rows;
}

/**
 * Build the usage/credits panel from the three served payloads plus the last
 * refusal. The `ok` state requires BOTH the entitlement snapshot and the
 * usage fold; a `billing_disabled` refusal renders "billing disabled locally"
 * with an explicit reason and NO numbers. Role-aware: the grant-credits
 * affordance is enabled only when the identity's effective actions carry
 * `credits_grant` (the server's admin role rule); it is visible-but-disabled
 * otherwise, with the precise reason.
 */
export function buildUsagePanel(input: CockpitUsageInput): CockpitUsagePanel {
  const { identity, entitlements, usage, refusal, cursor, hasPrev } = input;
  const page: CockpitUsagePageView = {
    cursor,
    nextCursor: usage === null ? null : usage.nextCursor,
    itemCount: usage === null ? 0 : usage.items.length,
    hasPrev,
  };
  const base = {
    organization: identity?.organization ?? entitlements?.organization_id ?? null,
    planId: entitlements?.plan_id ?? null,
    planFound: entitlements?.plan_found ?? false,
    subscription: subscriptionViewOf(entitlements),
    inFlight:
      entitlements === null
        ? []
        : entitlements.in_flight.map((txn) => ({
            id: txn.id,
            kind: txn.kind,
            reference: txn.reference,
            startedMs: txn.started_ms,
            endedMs: txn.ended_ms,
          })),
    page,
  };
  const canGrantCredits =
    identity !== null && identity.effective_actions.includes('credits_grant');
  const grantDisabledReason = canGrantCredits
    ? null
    : identity === null
      ? 'the control plane did not serve an identity; the granting role is unknown'
      : `the ${identity.role} role carries no credits_grant capability (admin only)`;
  const grantAction = (enabled: boolean): CockpitAction => ({
    key: 'grant-credits',
    label: 'Grant credits…',
    enabled,
  });
  const pageActions: CockpitAction[] = [
    { key: 'usage-prev', label: 'Previous page', enabled: hasPrev },
    { key: 'usage-next', label: 'Next page', enabled: usage !== null && usage.nextCursor !== null },
  ];
  if (refusal !== null) {
    const disabled = refusal.code === 'billing_disabled';
    return {
      state: disabled ? 'disabled' : 'unavailable',
      reason: refusal.reason,
      ...base,
      period: null,
      credits: null,
      quotas: [],
      canGrantCredits,
      grantDisabledReason: canGrantCredits
        ? 'the billing surface is disabled or unavailable; a grant cannot be attempted'
        : grantDisabledReason,
      actions: [grantAction(false), ...pageActions],
    };
  }
  if (entitlements === null || usage === null) {
    return {
      state: 'unavailable',
      reason: 'the control plane served no usage/entitlement payload',
      ...base,
      period: null,
      credits: null,
      quotas: [],
      canGrantCredits,
      grantDisabledReason: canGrantCredits
        ? 'the control plane served no usage/entitlement payload; a grant cannot be attempted'
        : grantDisabledReason,
      actions: [grantAction(false), ...pageActions],
    };
  }
  const totals = usageBucketView(usage.fold.totals);
  const totalTokens =
    totals.inputTokens +
    totals.outputTokens +
    totals.cacheReadTokens +
    totals.cacheWriteTokens +
    totals.reasoningTokens;
  const period: CockpitUsagePeriodView = {
    ...totals,
    totalTokens,
    tasks: usage.fold.per_task.map((task) => ({
      taskId: task.task_id,
      runId: task.run_id,
      totals: usageBucketView(task.totals),
    })),
    scanCursor: usage.fold.next_cursor,
  };
  return {
    state: 'ok',
    reason: null,
    ...base,
    period,
    credits: creditBalanceView(usage.credits),
    quotas: quotaRowsOf(entitlements, {
      tokens: entitlements.total_tokens,
      managedMicro: entitlements.managed_spend_micro,
      balanceMicro: microBalance(
        entitlements.credits.granted_micro,
        entitlements.credits.refunded_micro,
        entitlements.credits.consumed_micro,
      ),
    }),
    canGrantCredits,
    grantDisabledReason,
    actions: [grantAction(canGrantCredits), ...pageActions],
  };
}

/** Exact micro-USD display of one canonical decimal money string. */
function microText(value: string): string {
  return `${value}\u00b5$`;
}

/** One bounded line per quota row; `[EXCEEDED]` leads so it survives a clamp. */
export function usageQuotaLine(quota: CockpitUsageQuota): string {
  const head = quota.exceeded === true ? '[EXCEEDED] ' : '';
  if (quota.observed === null) {
    return `${head}quota ${quota.limit} — limit ${quota.value} (observed not served: ${quota.reason ?? 'unavailable'})`;
  }
  const unit = isMicroLimitName(quota.limit) ? microText : String;
  return `${head}quota ${quota.limit} — observed ${unit(quota.observed)} / limit ${quota.value}`;
}

/** The bounded section lines of the usage/credits panel. */
export function usagePanelLines(panel: CockpitUsagePanel): string[] {
  const lines: string[] = [];
  if (panel.state === 'disabled') {
    lines.push(`billing disabled locally${panel.reason === null ? '' : ` — ${panel.reason}`}`);
  } else if (panel.state === 'unavailable') {
    lines.push(`usage unavailable${panel.reason === null ? '' : ` — ${panel.reason}`}`);
  }
  if (panel.organization !== null) {
    lines.push(
      `organization ${panel.organization} · plan ${panel.planId ?? 'none'}${
        panel.planId !== null && !panel.planFound
          ? ' (plan not defined in the billing config — admission denies it)'
          : ''
      }`,
    );
  }
  const subscription = panel.subscription;
  const expires =
    subscription.expiresMs === null
      ? ''
      : ` (expires ${utcSeconds(subscription.expiresMs)})`;
  switch (subscription.state) {
    case 'active':
      lines.push(`subscription active${expires}`);
      break;
    case 'expired':
      lines.push(`[EXPIRED] subscription expired${expires} — new tasks are denied`);
      break;
    case 'canceled':
      lines.push(`[CANCELED] subscription canceled${expires}`);
      break;
    case 'grace':
      lines.push(
        `[GRACE] subscription lapsed: the durable row says active but the effective snapshot is inactive${expires} — new tasks are denied`,
      );
      break;
    default:
      lines.push('subscription unavailable (the snapshot serves no subscription)');
      break;
  }
  if (panel.period !== null) {
    lines.push(
      `period tokens — in ${panel.period.inputTokens} · out ${panel.period.outputTokens} · cache read ${panel.period.cacheReadTokens} · cache write ${panel.period.cacheWriteTokens} · reasoning ${panel.period.reasoningTokens} · total ${panel.period.totalTokens}`,
    );
    lines.push(
      `period spend — managed ${microText(panel.period.managedCostMicro)} · BYOK ${microText(panel.period.byokCostMicro)} · provider cost ${microText(panel.period.providerCostMicro)} · events ${panel.period.events} (corrected ${panel.period.correctedEvents})`,
    );
    for (const task of panel.period.tasks) {
      lines.push(
        `task ${task.taskId} (${task.runId}) — in ${task.totals.inputTokens} · out ${task.totals.outputTokens} · cache ${task.totals.cacheReadTokens}/${task.totals.cacheWriteTokens} · reasoning ${task.totals.reasoningTokens} · managed ${microText(task.totals.managedCostMicro)} · BYOK ${microText(task.totals.byokCostMicro)}`,
      );
    }
    if (panel.period.scanCursor !== null) {
      lines.push(
        `fold scan bound reached — fold cursor ${panel.period.scanCursor} (the fold is not exact past this cursor)`,
      );
    }
  }
  for (const quota of panel.quotas) {
    lines.push(usageQuotaLine(quota));
  }
  if (panel.credits !== null) {
    lines.push(
      `credits balance ${microText(panel.credits.balanceMicro)} — granted ${microText(panel.credits.grantedMicro)} · consumed ${microText(panel.credits.consumedMicro)} · refunded ${microText(panel.credits.refundedMicro)} · held ${microText(panel.credits.heldMicro)} · pending consumes ${panel.credits.pendingConsumes}`,
    );
  }
  for (const txn of panel.inFlight) {
    lines.push(
      `in-flight ${txn.kind} ${txn.id} (${txn.reference}) since ${utcSeconds(txn.startedMs)}${
        txn.endedMs === null ? '' : ` — ended ${utcSeconds(txn.endedMs)}`
      }`,
    );
  }
  lines.push(
    `usage events page — ${panel.page.itemCount} row(s) · cursor ${
      panel.page.cursor ?? 'first page'
    } · next ${panel.page.nextCursor ?? 'none'}`,
  );
  lines.push(
    panel.canGrantCredits
      ? 'role grants credits (credits_grant capability present)'
      : `grant credits disabled — ${panel.grantDisabledReason ?? 'not permitted'}`,
  );
  return lines.map((line) => clamp(line)).slice(0, MAX_LINES);
}

/**
 * The render plan of the usage/credits panel: one section carrying the
 * cursor controls and the role-gated grant affordance. A disabled action is
 * rendered disabled and never posts (the server role check is the gate).
 */
export function usagePanelSections(panel: CockpitUsagePanel): CockpitSection[] {
  return [
    {
      key: 'usage',
      title: 'Usage / Credits',
      present: panel.state === 'ok',
      lines: usagePanelLines(panel),
      evidence: [],
      actions: panel.actions,
    },
  ];
}

/** One state-gated control a cockpit section renders. */
export interface CockpitAction {
  readonly key: 'decide' | 'abort' | 'usage-prev' | 'usage-next' | 'grant-credits';
  readonly label: string;
  readonly enabled: boolean;
}

export interface CockpitSection {
  readonly key: string;
  readonly title: string;
  readonly present: boolean;
  readonly lines: readonly string[];
  readonly evidence: readonly CockpitEvidenceRef[];
  /**
   * The structured per-criterion proof rows of the acceptance section (the
   * chat webview renders these; older snapshots fall back to the
   * `[verdict]`-led lines).
   */
  readonly criteria?: readonly CockpitCriterionProof[];
  /** State-gated controls (tournament decide/abort), when the section owns any. */
  readonly actions?: readonly CockpitAction[];
}

/** Minimal structural view of the durable native tournament payload. */
export interface CockpitNativeTournament {
  readonly id: string;
  readonly state: string;
  readonly winner: string | null;
  readonly criteria: readonly { readonly id: string; readonly spec: string }[];
  readonly candidates: readonly {
    readonly childId: string;
    readonly state: string;
    readonly verification: number | null;
    readonly verificationPass: boolean | null;
    readonly reviewRank: string | null;
    readonly reviewer: string | null;
    readonly costMicro: MicroMoney;
    readonly wallMs: number;
  }[];
}

/**
 * Pure tournament projection: `open` while the engine can still act and
 * `canDecide` only once every candidate has settled (the engine's own rule).
 * Integration is never automatic — a winner is a proposal.
 */
export function tournamentViewOf(
  tournament: CockpitNativeTournament | null,
): CockpitTournamentView | null {
  if (tournament === null) {
    return null;
  }
  const open = tournament.state === 'open' || tournament.state === 'deciding';
  const candidates = tournament.candidates.map((candidate) => ({
    childId: candidate.childId,
    state: candidate.state,
    verification: candidate.verification,
    verificationPass: candidate.verificationPass,
    reviewRank: candidate.reviewRank,
    reviewer: candidate.reviewer,
    costMicro: microToString(candidate.costMicro),
    wallMs: candidate.wallMs,
    winner: tournament.winner !== null && candidate.childId === tournament.winner,
  }));
  return {
    id: tournament.id,
    state: tournament.state,
    open,
    canDecide: open && candidates.length > 0 && candidates.every((c) => c.state !== 'running'),
    winner: tournament.winner,
    criteria: tournament.criteria.map((criterion) => criterion.spec),
    candidates,
  };
}

/**
 * Minimal structural view of the task-verification wire payload. The
 * required record members are the route's frozen contract; every proof
 * annotation is optional and its absence is an honest `null`. The CURRENT
 * daemon serves the per-criterion `binding`/`origin`/`requirement`/`verdict`
 * annotations, the `candidateProof` family plus the record-level
 * `verifiedSnapshot`/`basedOnSnapshot`/`landedSnapshot`/`sourceCount`, and
 * the reviewer; the published commit and the remote PR head render when the
 * daemon publishes them (see crates/server/src/native/verification.rs).
 */
export interface CockpitTaskVerification {
  readonly records: readonly {
    readonly recordId: string;
    readonly status: string;
    readonly startedMs: number;
    readonly completedMs: number | null;
    readonly treeHash?: string | null;
    /** The independent reviewer identity, when one judged the record. */
    readonly reviewer?: string | null;
    readonly criteria: readonly {
      readonly criterionKey: string;
      readonly passed: boolean | null;
      readonly evidence: string | null;
      readonly requirement?: string | null;
      readonly origin?: string | null;
      readonly verdict?: string | null;
      readonly binding?: {
        readonly kind: string;
        readonly servedKind?: string | null;
        readonly checkId?: string | null;
        readonly commandDigest?: string | null;
        readonly requiredWorkItems?: readonly string[];
        readonly path?: string | null;
        readonly expectedDigest?: string | null;
        readonly evidenceId?: string | null;
        readonly evidenceDigest?: string | null;
        readonly reviewerId?: string | null;
        readonly reason?: string | null;
      } | null;
    }[];
    readonly candidateProof?: {
      readonly taskRevision?: string | null;
      readonly baseManifestHash?: string | null;
      readonly candidateManifestHash?: string | null;
      readonly sourceDiffEvidence?: string | null;
      readonly riskReportEvidence?: string | null;
      readonly accountingSnapshotDigest?: string | null;
      readonly runId?: string | null;
      readonly runBaseSnapshot?: string | null;
      readonly candidateSnapshot?: string | null;
      readonly sourcesDigest?: string | null;
      readonly changedFilesDigest?: string | null;
      /** The published commit OID, when the candidate was committed. */
      readonly publishedCommit?: string | null;
      /** The remote PR head ref/OID, when a PR was opened for the candidate. */
      readonly remotePrHead?: string | null;
    } | null;
    readonly verifiedSnapshot?: string | null;
    readonly basedOnSnapshot?: string | null;
    readonly sourceCount?: number | null;
    readonly landedSnapshot?: string | null;
    readonly checks: readonly {
      readonly check: string;
      readonly status: string;
      readonly required: boolean;
    }[];
  }[];
}

export interface CockpitInput {
  readonly task: TaskSummary | null;
  readonly agents: readonly AgentSummary[];
  readonly verification: VerificationSummary | null;
  readonly usage: UsageSummary | null;
  readonly taskVerification: CockpitTaskVerification | null;
  /** The durable tournament of the session, when one exists. */
  readonly tournament?: CockpitTournamentView | null;
  /** The strict proof summary for the top-level VERIFIED view. */
  readonly proof?: CockpitNativeProof | null;
  /**
   * A proof fetch/validation refusal. Renders as an explicit
   * `unavailable` proof state; never silently ignored, never VERIFIED.
   */
  readonly proofUnavailable?: string | null;
}

const MAX_LINES = 64;
const MAX_TEXT = 240;

function clamp(value: string): string {
  return value.length > MAX_TEXT ? `${value.slice(0, MAX_TEXT)}…` : value;
}

function textOf(value: unknown): string | null {
  return typeof value === 'string' && value.trim().length > 0 ? clamp(value.trim()) : null;
}

/** Extract a current-phase label from a native progress record, if any. */
export function phaseOf(progress: unknown): string | null {
  if (typeof progress !== 'object' || progress === null || Array.isArray(progress)) {
    return null;
  }
  const record = progress as Record<string, unknown>;
  for (const key of ['phase', 'stage', 'current_phase', 'currentPhase', 'label']) {
    const value = textOf(record[key]);
    if (value !== null) {
      return value;
    }
  }
  return null;
}

function blockersOf(progress: unknown): string[] {
  if (typeof progress !== 'object' || progress === null || Array.isArray(progress)) {
    return [];
  }
  const record = progress as Record<string, unknown>;
  for (const key of ['blockers', 'blocked_on', 'blockedOn', 'blocker']) {
    const value = record[key];
    if (typeof value === 'string' && value.trim().length > 0) {
      return [clamp(value.trim())];
    }
    if (Array.isArray(value)) {
      return value
        .map((entry) => {
          if (typeof entry === 'string') {
            return textOf(entry);
          }
          if (typeof entry === 'object' && entry !== null) {
            const object = entry as Record<string, unknown>;
            return (
              textOf(object.detail) ??
              textOf(object.message) ??
              textOf(object.summary) ??
              textOf(object.id)
            );
          }
          return null;
        })
        .filter((entry): entry is string => entry !== null)
        .slice(0, MAX_LINES);
    }
  }
  return [];
}

/** Parse `evidence:41`, `evidence/41` or `#41` refs; free text is kept. */
export function evidenceRefOf(raw: string): CockpitEvidenceRef {
  const match = /(?:^|[^\w])evidence[:#/](\d+)(?![\d])/.exec(raw);
  return { id: match ? Number(match[1]) : null, label: clamp(raw) };
}

function boundedJson(value: unknown): string | null {
  if (value === null || value === undefined) {
    return null;
  }
  try {
    const encoded = JSON.stringify(value);
    return encoded === undefined ? null : clamp(encoded);
  } catch {
    return null;
  }
}

// ------------------------------------------------- acceptance proof builder

/** Hard bound of proof rows the acceptance section renders. */
export const MAX_CRITERIA_PROOF = 32;

const CRITERION_BINDING_KINDS: readonly CriterionBindingView[] = [
  'required_check',
  'integration_coverage',
  'file_state',
  'evidence',
  'independent_review',
  'aggregate_goal',
  'unavailable',
];

const REQUIREMENT_VIEWS: Record<string, CriterionRequirementView> = {
  required: 'required',
  preferred: 'preferred',
  advisory: 'preferred',
};

const ORIGIN_VIEWS: Record<string, CriterionOriginView> = {
  user: 'user',
  project_policy: 'project_policy',
  verification_policy: 'verification_policy',
  semantic_provider: 'semantic_provider',
};

function textOrNull(value: string | null | undefined): string | null {
  return typeof value === 'string' && value.trim().length > 0 ? clamp(value.trim()) : null;
}

/**
 * One bounded reviewer identity out of the durable reviewer JSON: a bare
 * string, or an object carrying `id`/`name`/`reviewer`. Anything else is an
 * honest null (never a fabricated identity).
 */
function reviewerTextOf(value: unknown): string | null {
  if (typeof value === 'string') {
    return textOrNull(value);
  }
  if (typeof value === 'object' && value !== null && !Array.isArray(value)) {
    const record = value as Record<string, unknown>;
    for (const key of ['id', 'name', 'reviewer', 'reviewerId', 'reviewer_id']) {
      const text = textOrNull(typeof record[key] === 'string' ? (record[key] as string) : null);
      if (text !== null) {
        return text;
      }
    }
  }
  return null;
}

/** A short (still honest) display form of one snapshot digest for lines. */
function digestLabel(value: string | null): string | null {
  const bounded = textOrNull(value);
  if (bounded === null) {
    return null;
  }
  return bounded.length > 8 ? `${bounded.slice(0, 8)}…` : bounded;
}

/** Second-precision UTC of one epoch-ms stamp (bounded line rendering). */
function utcSeconds(ms: number): string {
  return `${new Date(ms).toISOString().slice(0, 19)}Z`;
}

/** Split the record's evidence string into its typed refs (bounded). */
function evidenceRefsOf(raw: string | null): CockpitEvidenceRef[] {
  if (raw === null || raw.trim().length === 0) {
    return [];
  }
  const out: CockpitEvidenceRef[] = [];
  const seen = new Set<string>();
  const push = (entry: string): void => {
    const trimmed = entry.trim();
    if (trimmed.length === 0 || out.length >= MAX_LINES) {
      return;
    }
    const ref = evidenceRefOf(trimmed);
    const dedupe = `${ref.id ?? 'text'}:${ref.label}`;
    if (!seen.has(dedupe)) {
      seen.add(dedupe);
      out.push(ref);
    }
  };
  for (const segment of raw.split(';')) {
    push(segment);
  }
  return out;
}

interface DerivedBinding {
  readonly kind: CriterionBindingView;
  readonly reference: string | null;
  readonly detail: string | null;
  readonly reason: string;
}

/**
 * Read the binding kind from the criterion's TYPED evidence refs. This is a
 * deterministic projection of served data (never a guess): `check:` refs are
 * required-check evidence, `file:` refs are file-state evidence, `work-item:`
 * refs are integration-coverage evidence and a lone `evidence:<n>` ref is an
 * evidence binding. Anything else (mixed kinds, reviewer citations,
 * aggregate concatenations) is explicitly unavailable.
 */
function deriveBinding(rawRefs: readonly string[]): DerivedBinding | null {
  const refs = rawRefs.map((entry) => entry.trim()).filter((entry) => entry.length > 0);
  if (refs.length === 0) {
    return null;
  }
  if (refs.length === 1 && refs[0]!.startsWith('check:')) {
    const rest = refs[0]!.slice('check:'.length);
    const cut = rest.lastIndexOf(':');
    if (cut > 0) {
      return {
        kind: 'required_check',
        reference: refs[0]!,
        detail: `check ${rest.slice(0, cut)} · command digest ${rest.slice(cut + 1)}`,
        reason: 'derived from the typed check evidence ref',
      };
    }
    return {
      kind: 'required_check',
      reference: refs[0]!,
      detail: `check ${rest}`,
      reason: 'derived from the typed check evidence ref',
    };
  }
  if (refs.length === 1 && refs[0]!.startsWith('file:')) {
    return {
      kind: 'file_state',
      reference: refs[0]!,
      detail: `path ${refs[0]!.slice('file:'.length)}`,
      reason: 'derived from the typed file evidence ref',
    };
  }
  if (refs.every((entry) => entry.startsWith('work-item:'))) {
    return {
      kind: 'integration_coverage',
      reference: refs.join(', '),
      detail: `${refs.length} work-item contribution(s)`,
      reason: 'derived from the typed work-item evidence refs',
    };
  }
  if (refs.length === 1 && /^evidence:\d+$/.test(refs[0]!)) {
    return {
      kind: 'evidence',
      reference: refs[0]!,
      detail: `evidence ${refs[0]!.slice('evidence:'.length)}`,
      reason: 'derived from the typed immutable-evidence ref',
    };
  }
  return null;
}

interface ServedBindingInput {
  readonly kind: string;
  readonly servedKind?: string | null;
  readonly checkId?: string | null;
  readonly commandDigest?: string | null;
  readonly requiredWorkItems?: readonly string[];
  readonly path?: string | null;
  readonly expectedDigest?: string | null;
  readonly evidenceId?: string | null;
  readonly evidenceDigest?: string | null;
  readonly reviewerId?: string | null;
  readonly reason?: string | null;
}

function servedBindingView(binding: ServedBindingInput): {
  readonly kind: CriterionBindingView;
  readonly reference: string | null;
  readonly detail: string | null;
} {
  const kind = (CRITERION_BINDING_KINDS as readonly string[]).includes(binding.kind)
    ? (binding.kind as CriterionBindingView)
    : 'unavailable';
  const text = (value: string | null | undefined): string | null => textOrNull(value);
  switch (kind) {
    case 'required_check': {
      const checkId = text(binding.checkId);
      const digest = text(binding.commandDigest);
      const reference = checkId !== null ? `check:${checkId}${digest !== null ? `:${digest}` : ''}` : null;
      return {
        kind,
        reference,
        detail: checkId !== null ? `check ${checkId}${digest !== null ? ` · command digest ${digest}` : ''}` : null,
      };
    }
    case 'integration_coverage': {
      const items = (binding.requiredWorkItems ?? []).map((item) => item.trim()).filter((item) => item.length > 0);
      return {
        kind,
        reference: items.length > 0 ? `work-item:${items.join(', work-item:')}` : null,
        detail: items.length > 0 ? `${items.length} required work item(s)` : null,
      };
    }
    case 'file_state':
      return {
        kind,
        reference: text(binding.path),
        detail: text(binding.expectedDigest) !== null ? `expected digest ${text(binding.expectedDigest)}` : null,
      };
    case 'evidence':
      return {
        kind,
        reference: text(binding.evidenceId) !== null ? `evidence:${text(binding.evidenceId)}` : null,
        detail: text(binding.evidenceDigest) !== null ? `evidence digest ${text(binding.evidenceDigest)}` : null,
      };
    case 'independent_review':
      return {
        kind,
        reference: text(binding.reviewerId),
        detail: text(binding.reviewerId) !== null ? `reviewer ${text(binding.reviewerId)}` : null,
      };
    case 'aggregate_goal':
      return { kind, reference: null, detail: 'all subordinate criteria + the independent final review' };
    case 'unavailable':
    default:
      return {
        kind: 'unavailable',
        reference: null,
        detail: text(binding.reason) ?? text(binding.servedKind),
      };
  }
}

function requirementOf(criterion: {
  readonly requirement?: string | null;
}): { readonly requirement: CriterionRequirementView; readonly reason: string | null } {
  const raw = criterion.requirement;
  if (typeof raw === 'string' && raw in REQUIREMENT_VIEWS) {
    return { requirement: REQUIREMENT_VIEWS[raw]!, reason: null };
  }
  return {
    requirement: 'unavailable',
    reason:
      raw === null || raw === undefined
        ? 'this row carries no criterion requirement (required/advisory) member'
        : `unrecognized criterion requirement ${JSON.stringify(raw)} served; no global suite status substitutes`,
  };
}

function originOf(criterion: {
  readonly origin?: string | null;
}): { readonly origin: CriterionOriginView; readonly reason: string | null } {
  const raw = criterion.origin;
  if (typeof raw === 'string' && raw in ORIGIN_VIEWS) {
    return { origin: ORIGIN_VIEWS[raw]!, reason: null };
  }
  return {
    origin: 'unavailable',
    reason:
      raw === null || raw === undefined
        ? 'this row carries no criterion origin member'
        : `unrecognized criterion origin ${JSON.stringify(raw)} served`,
  };
}

/** Map the served verdict vocabulary (or the recorded boolean). */
function verdictOf(criterion: {
  readonly passed: boolean | null;
  readonly verdict?: string | null;
}): {
  readonly verdict: CriterionVerdictView;
  readonly source: CriterionVerdictSource;
  readonly reason: string | null;
} {
  const explicit = criterion.verdict;
  if (explicit === 'passed' || explicit === 'pass') {
    return { verdict: 'pass', source: 'daemon', reason: null };
  }
  if (explicit === 'failed' || explicit === 'fail') {
    return { verdict: 'fail', source: 'daemon', reason: null };
  }
  if (explicit === 'unavailable') {
    return { verdict: 'unavailable', source: 'daemon', reason: null };
  }
  if (criterion.passed === true) {
    return { verdict: 'pass', source: 'recorded', reason: null };
  }
  if (criterion.passed === false) {
    return {
      verdict: 'fail',
      source: 'recorded',
      reason:
        'recorded as not passed; this row carries no three-way verdict, so failed and unavailable cannot be distinguished here',
    };
  }
  return {
    verdict: 'unavailable',
    source: 'unavailable',
    reason: 'no verdict fact was served for this criterion row',
  };
}

type VerificationRecordInput = CockpitTaskVerification['records'][number];
type CriterionInput = VerificationRecordInput['criteria'][number];

function snapshotOf(record: VerificationRecordInput | null): CockpitProofSnapshotView {
  const proof = record?.candidateProof ?? null;
  return {
    candidate: textOrNull(proof?.candidateSnapshot),
    verified: textOrNull(record?.verifiedSnapshot) ?? textOrNull(record?.treeHash),
    basedOn: textOrNull(record?.basedOnSnapshot),
    landed: textOrNull(record?.landedSnapshot),
    sourceCount: typeof record?.sourceCount === 'number' ? record.sourceCount : null,
    runId: textOrNull(proof?.runId),
    taskRevision: textOrNull(proof?.taskRevision),
    baseManifestHash: textOrNull(proof?.baseManifestHash),
    candidateManifestHash: textOrNull(proof?.candidateManifestHash),
    sourcesDigest: textOrNull(proof?.sourcesDigest),
    changedFilesDigest: textOrNull(proof?.changedFilesDigest),
    accountingSnapshotDigest: textOrNull(proof?.accountingSnapshotDigest),
    sourceDiffEvidence: textOrNull(proof?.sourceDiffEvidence),
    riskReportEvidence: textOrNull(proof?.riskReportEvidence),
    publishedCommit: textOrNull(proof?.publishedCommit),
    remotePrHead: textOrNull(proof?.remotePrHead),
  };
}

/**
 * One criterion proof row. Every missing member is reported as unavailable
 * with its reason; the verdict is NEVER pass without a pass fact and the
 * binding kind is never invented.
 */
export function criterionProofRow(
  criterionKey: string,
  criterion: CriterionInput | null,
  record: VerificationRecordInput | null,
): CockpitCriterionProof {
  const evidenceRaw = criterion?.evidence ?? null;
  const refs = evidenceRefsOf(evidenceRaw);
  const rawRefs = refs.map((ref) => ref.label);
  const requirement = requirementOf(criterion ?? {});
  const origin = originOf(criterion ?? {});
  const unavailable: string[] = [];
  if (requirement.reason !== null) {
    unavailable.push('requirement');
  }
  if (origin.reason !== null) {
    unavailable.push('origin');
  }

  let binding: CriterionBindingView = 'unavailable';
  let bindingSource: CriterionBindingSource = 'unavailable';
  let bindingReference: string | null = null;
  let bindingDetail: string | null = null;
  if (criterion?.binding != null) {
    const served = servedBindingView(criterion.binding);
    binding = served.kind;
    bindingSource = 'daemon';
    bindingReference = served.reference;
    bindingDetail = served.detail;
    if (binding === 'unavailable') {
      unavailable.push('binding');
      bindingDetail = served.detail ?? 'the served binding is explicitly unavailable';
    }
  } else if (criterion === null) {
    bindingDetail = 'no verification record covers this criterion';
    unavailable.push('binding');
  } else {
    const derived = deriveBinding(rawRefs);
    if (derived !== null) {
      binding = derived.kind;
      bindingSource = 'derived';
      bindingReference = derived.reference;
      bindingDetail = derived.detail;
    } else {
      bindingDetail =
        refs.length > 0
          ? 'this row carries no criterion binding and the evidence refs do not identify one kind'
          : 'this row carries no criterion binding';
      unavailable.push('binding');
    }
  }

  const verdict = verdictOf(criterion ?? { passed: null });
  if (verdict.source === 'unavailable') {
    unavailable.push('verdict');
  }

  const snapshot = snapshotOf(record);
  const hasSnapshot =
    snapshot.candidate !== null ||
    snapshot.verified !== null ||
    snapshot.basedOn !== null ||
    snapshot.landed !== null ||
    snapshot.sourceCount !== null ||
    snapshot.publishedCommit !== null ||
    snapshot.remotePrHead !== null;
  if (!hasSnapshot) {
    unavailable.push('snapshots');
  }
  const timestamp: CockpitProofTimestamp = {
    startedMs: typeof record?.startedMs === 'number' ? record.startedMs : null,
    completedMs: typeof record?.completedMs === 'number' ? record.completedMs : null,
  };
  if (timestamp.startedMs === null && timestamp.completedMs === null) {
    unavailable.push('verification timestamp');
  }

  return {
    criterionKey: textOrNull(criterionKey) ?? '',
    requirement: requirement.requirement,
    requirementReason: requirement.reason,
    origin: origin.origin,
    originReason: origin.reason,
    binding,
    bindingSource,
    bindingReference,
    bindingDetail,
    verdict: verdict.verdict,
    verdictSource: verdict.source,
    verdictReason: verdict.reason,
    evidenceRefs: refs,
    snapshot,
    timestamp,
    recordId: record !== null ? textOrNull(record.recordId) : null,
    recordStatus: record !== null ? textOrNull(record.status) : null,
    unavailable: unavailable.slice(0, MAX_LINES),
  };
}

/** Build the acceptance-proof rows of one task (explicit criteria first). */
export function buildCriteriaProof(input: {
  readonly acceptanceCriteria?: readonly string[] | null;
  readonly taskVerification?: CockpitTaskVerification | null;
}): CockpitCriterionProof[] {
  const records = input.taskVerification?.records ?? [];
  const explicit = (input.acceptanceCriteria ?? [])
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
  const rows: CockpitCriterionProof[] = [];
  const explicitSet = new Set(explicit);
  const newest = records.length > 0 ? records[0]! : null;
  for (const key of explicit) {
    let matchedRecord: VerificationRecordInput | null = null;
    let matchedCriterion: CriterionInput | null = null;
    for (const record of records) {
      const criterion = record.criteria.find((entry) => entry.criterionKey === key);
      if (criterion !== undefined) {
        matchedRecord = record;
        matchedCriterion = criterion;
        break;
      }
    }
    rows.push(criterionProofRow(key, matchedCriterion, matchedRecord ?? newest));
    if (rows.length >= MAX_CRITERIA_PROOF) {
      return rows;
    }
  }
  for (const record of records) {
    for (const criterion of record.criteria) {
      if (criterion.criterionKey !== '' && explicitSet.has(criterion.criterionKey)) {
        continue;
      }
      rows.push(criterionProofRow(criterion.criterionKey, criterion, record));
      if (rows.length >= MAX_CRITERIA_PROOF) {
        return rows;
      }
    }
  }
  return rows;
}

/** One bounded human line per criterion; `[verdict]` leads so it survives a clamp. */
export function criterionProofLine(row: CockpitCriterionProof): string {
  // Long criterion prose is shortened so the proof facts (binding, snapshot,
  // timestamps) always survive the bridge's 240-char line clamp; the
  // structured row keeps the full text.
  const key =
    row.criterionKey.length === 0
      ? '(unnamed criterion — malformed row)'
      : row.criterionKey.length > 60
        ? `${row.criterionKey.slice(0, 60)}…`
        : row.criterionKey;
  const parts: string[] = [`[${row.verdict}] ${key}`];
  // Requirement/origin render their honest availability label here; the full
  // reason text stays on the structured row (it would overflow the bridge's
  // 240-char line clamp and push the snapshot facts out of the overlay).
  parts.push(row.requirement);
  parts.push(row.origin);
  const bindingRef = row.bindingReference !== null ? ` ref ${row.bindingReference}` : '';
  parts.push(
    `binding ${row.binding}${row.bindingSource === 'derived' ? ' (derived)' : ''}${bindingRef}`,
  );
  const snapshotBits = [
    row.snapshot.candidate !== null ? `cand ${digestLabel(row.snapshot.candidate)}` : null,
    row.snapshot.verified !== null ? `ver ${digestLabel(row.snapshot.verified)}` : null,
    row.snapshot.basedOn !== null ? `base ${digestLabel(row.snapshot.basedOn)}` : null,
    row.snapshot.landed !== null ? `land ${digestLabel(row.snapshot.landed)}` : null,
    row.snapshot.sourceCount !== null ? `src ${row.snapshot.sourceCount}` : null,
    row.snapshot.publishedCommit !== null
      ? `commit ${digestLabel(row.snapshot.publishedCommit)}`
      : null,
    row.snapshot.remotePrHead !== null ? `head ${digestLabel(row.snapshot.remotePrHead)}` : null,
  ].filter((entry): entry is string => entry !== null);
  parts.push(snapshotBits.length > 0 ? snapshotBits.join(' · ') : 'snapshot unavailable');
  const atBits = [
    row.timestamp.startedMs !== null ? utcSeconds(row.timestamp.startedMs) : null,
    row.timestamp.completedMs !== null ? utcSeconds(row.timestamp.completedMs) : null,
  ].filter((entry): entry is string => entry !== null);
  parts.push(atBits.length > 0 ? `at ${atBits.join(' → ')}` : 'timestamp unavailable');
  const evidenceIds = row.evidenceRefs
    .map((ref) => ref.id)
    .filter((id): id is number => id !== null)
    .slice(0, 8)
    .map((id) => `evidence:${id}`);
  if (evidenceIds.length > 0) {
    parts.push(`ev ${evidenceIds.join(' ')}`);
  }
  return clamp(parts.join(' · '));
}

export function buildCockpit(input: CockpitInput): CockpitView | null {
  const { task, agents, verification, usage, taskVerification } = input;
  const tournament = input.tournament ?? null;
  // Background children are TUCKED: they render after the foreground ones
  // (the durable presentation field decides; never the state heuristics).
  const allChildren = agents.filter((agent) => agent.kind === 'child');
  const children = allChildren.filter((child) => child.presentation !== 'background').concat(
    allChildren.filter((child) => child.presentation === 'background'),
  );
  if (task === null && children.length === 0 && verification === null && tournament === null) {
    return null;
  }

  // Acceptance criteria: the task row's explicit list first; otherwise the
  // verification record's criterion keys (never invented). The per-criterion
  // PROOF rows carry the binding/verdict/reference/snapshot/timestamp facts
  // and degrade honestly when the payload does not serve one.
  const explicitCriteria = (task?.acceptanceCriteria ?? []).filter(
    (entry) => entry.trim().length > 0,
  );
  const recordCriteria = (taskVerification?.records ?? []).flatMap((record) =>
    record.criteria
      .map((criterion) => criterion.criterionKey)
      .filter((key): key is string => typeof key === 'string' && key.trim().length > 0),
  );
  const acceptanceCriteria = (explicitCriteria.length > 0 ? explicitCriteria : recordCriteria).slice(
    0,
    MAX_LINES,
  );
  const criteriaProof = buildCriteriaProof({
    acceptanceCriteria: explicitCriteria,
    taskVerification,
  });

  // Plan / DAG steps: explicit plan steps when served, else the milestone
  // lists; children link by item id.
  const planSteps = task?.plan ?? [];
  const steps: CockpitStepView[] = planSteps.map((step) => ({
    id: step.id,
    summary: step.summary,
    status: step.state,
    dependsOn: step.dependsOn,
    childIds: children.filter((child) => child.itemId === step.id).map((child) => child.agentId),
  }));
  const plannedIds = new Set(steps.map((step) => step.id));
  for (const completed of task?.completed ?? []) {
    if (!plannedIds.has(completed)) {
      steps.push({
        id: completed,
        summary: completed,
        status: 'done',
        dependsOn: [],
        childIds: children.filter((child) => child.itemId === completed).map((child) => child.agentId),
      });
    }
  }
  for (const open of task?.open ?? []) {
    if (!plannedIds.has(open)) {
      steps.push({
        id: open,
        summary: open,
        status: 'open',
        dependsOn: [],
        childIds: children.filter((child) => child.itemId === open).map((child) => child.agentId),
      });
    }
  }

  // Blockers: explicit additive fields on the task view first, then the
  // progress record, then visibly-blocked children.
  const blockers: string[] = [];
  for (const explicit of task?.blockers ?? []) {
    blockers.push(clamp(explicit));
  }
  for (const derived of blockersOf(task?.progress)) {
    blockers.push(derived);
  }
  for (const child of children) {
    if (child.state.trim().toLowerCase() === 'blocked') {
      blockers.push(`child ${child.agentId} is blocked on ${child.itemId ?? 'its work item'}`);
    }
  }

  // Verification: the durable record summary when available, else the
  // session verification view.
  let criteriaPassed = 0;
  let criteriaTotal = 0;
  let checksFailed = 0;
  let recordStatus: string | null = null;
  let topRecord: VerificationRecordInput | null = null;
  for (const record of taskVerification?.records ?? []) {
    recordStatus = record.status;
    topRecord = record;
    for (const criterion of record.criteria) {
      criteriaTotal += 1;
      if (criterion.passed) {
        criteriaPassed += 1;
      }
    }
    for (const check of record.checks) {
      if (check.status.toLowerCase() === 'failed') {
        checksFailed += 1;
      }
    }
  }
  const topSnapshot = snapshotOf(topRecord);
  const owed = verification?.owed.length ?? 0;
  const failedChecks = verification?.failedChecks.length ?? 0;
  const verificationView: CockpitVerificationView = {
    status:
      recordStatus ??
      (criteriaTotal > 0 && criteriaPassed === criteriaTotal
        ? 'passed'
        : failedChecks > 0 || checksFailed > 0
          ? 'failed'
          : owed > 0
            ? 'pending'
            : task?.state ?? 'unknown'),
    criteriaPassed,
    criteriaTotal,
    checksFailed,
    owed,
    failedChecks,
    verified:
      topRecord !== null &&
      topRecord.status.toLowerCase() === 'passed' &&
      criteriaTotal > 0 &&
      criteriaPassed === criteriaTotal &&
      checksFailed === 0,
    reviewer: textOrNull(topRecord?.reviewer),
    tree: topSnapshot.verified,
    publishedCommit: topSnapshot.publishedCommit,
    remotePrHead: topSnapshot.remotePrHead,
  };

  // Evidence refs: verification criteria evidence + check summaries + any
  // explicit refs carried on the task view.
  const evidence: CockpitEvidenceRef[] = [];
  const seenEvidence = new Set<string>();
  const pushEvidence = (raw: string | null | undefined): void => {
    if (typeof raw !== 'string' || raw.trim().length === 0) {
      return;
    }
    const ref = evidenceRefOf(raw.trim());
    const dedupe = `${ref.id ?? 'text'}:${ref.label}`;
    if (!seenEvidence.has(dedupe) && evidence.length < MAX_LINES) {
      seenEvidence.add(dedupe);
      evidence.push(ref);
    }
  };
  for (const record of taskVerification?.records ?? []) {
    for (const criterion of record.criteria) {
      pushEvidence(criterion.evidence);
    }
  }
  for (const ref of task?.evidenceRefs ?? []) {
    pushEvidence(ref);
  }
  // Typed proof refs (check/file/work-item/evidence) stay retrievable in the
  // evidence section too; non-evidence ids render as bounded text.
  for (const row of criteriaProof) {
    for (const ref of row.evidenceRefs) {
      pushEvidence(ref.label);
    }
  }

  const spend: CockpitSpendView | null = usage
    ? {
        spentTokens: usage.tokens,
        maxTokens: task?.budget?.maxTokens ?? null,
        spentCostMicro: usage.spentMicro,
        maxCostMicro: usage.maxMicro,
        openReservedMicro: usage.openMicro,
      }
    : task?.budget
      ? {
          spentTokens: task.budget.spentTokens,
          maxTokens: task.budget.maxTokens,
          spentCostMicro: task.budget.spentCostMicro,
          maxCostMicro: task.budget.maxCostMicro,
          openReservedMicro: task.budget.openReservedMicro,
        }
      : null;

  return {
    goal: task?.goal ?? children[0]?.goal ?? '',
    state: task?.state ?? 'unknown',
    phase: task?.phase ?? phaseOf(task?.progress) ?? task?.state ?? 'unknown',
    acceptanceCriteria,
    criteriaProof,
    steps,
    children: children.map((child) => ({
      childId: child.agentId,
      state: child.state,
      itemId: child.itemId,
      itemKind: child.itemKind,
      worktreeId: child.worktreeId,
      ownership: child.ownership,
      model: child.model,
      provider: child.provider,
      reasoning: child.reasoning,
      thinking: child.thinking,
      budget: child.budget,
      progress: boundedJson(child.progress),
      result: boundedJson(child.result),
      presentation: child.presentation === 'background' ? 'background' : 'foreground',
    })),
    blockers,
    verification: verificationView,
    evidence,
    spend,
    tournament,
    completion: task?.completion ?? null,
    proof: proofViewOf(input.proof ?? null, input.proofUnavailable ?? null),
  };
}

/** The render plan of the persistent cockpit panel, in fixed order. */
export function cockpitSections(view: CockpitView): CockpitSection[] {
  const sections: CockpitSection[] = [];
  const proofLines = view.criteriaProof
    .slice(0, MAX_CRITERIA_PROOF)
    .map((row) => criterionProofLine(row));
  const acceptanceLines =
    proofLines.length > 0
      ? proofLines
      : view.acceptanceCriteria.length > 0
        ? view.acceptanceCriteria.map((criterion) => `• ${criterion}`)
        : ['none'];
  sections.push({
    key: 'acceptance',
    title: 'Acceptance criteria · proof',
    present: view.criteriaProof.length > 0 || view.acceptanceCriteria.length > 0,
    lines: acceptanceLines.slice(0, MAX_LINES),
    evidence: [],
    criteria: view.criteriaProof.slice(0, MAX_CRITERIA_PROOF),
  });
  sections.push({
    key: 'proof',
    title: 'Verification proof (daemon)',
    present: view.proof !== null,
    lines:
      view.proof === null
        ? ['not fetched']
        : [
            proofSummaryLine(view.proof),
            ...(view.proof.reason === null ? [] : [`reason: ${view.proof.reason}`]),
            ...view.proof.unavailable.map(
              (entry) => `${entry.component} (${entry.kind}): ${entry.reason}`,
            ),
            ...(view.proof.trees.verified === null
              ? []
              : [
                  `verified tree ${shortRef(view.proof.trees.verified)} · landed ${
                    view.proof.trees.landed === null
                      ? 'none'
                      : shortRef(view.proof.trees.landed)
                  }`,
                ]),
            ...proofStepLines(view.proof),
          ].slice(0, MAX_LINES),
    evidence: [],
  });
  sections.push({
    key: 'plan',
    title: 'Plan / DAG steps',
    present: view.steps.length > 0,
    lines:
      view.steps.length > 0
        ? view.steps.map((step) => {
            const deps = step.dependsOn.length > 0 ? ` (after ${step.dependsOn.join(', ')})` : '';
            const kids = step.childIds.length > 0 ? ` → ${step.childIds.join(', ')}` : '';
            return `[${step.status}] ${step.id}: ${step.summary}${deps}${kids}`;
          })
        : ['none'],
    evidence: [],
  });
  sections.push({
    key: 'completion',
    title: 'Completion contract',
    present: view.completion !== null,
    lines:
      view.completion === null
        ? ['none (plain task; no conditional commit/push/PR steps)']
        : [
            `requested: ${
              [
                view.completion.includeCommit ? 'commit' : null,
                view.completion.includePush ? 'push' : null,
                view.completion.includePr ? 'pr' : null,
              ]
                .filter((step): step is string => step !== null)
                .join(', ') || 'none'
            }`,
            ...view.completion.steps.map(
              (step) =>
                `[${step.status}] ${step.step}${
                  step.detail !== null && step.detail.length > 0 ? ` — ${step.detail}` : ''
                }`,
            ),
            `status source: ${view.completion.source}${
              view.completion.reason !== null ? ` (${view.completion.reason})` : ''
            }`,
          ],
    evidence: [],
  });
  sections.push({
    key: 'children',
    title: 'Children',
    present: view.children.length > 0,
    lines:
      view.children.length > 0
        ? view.children.map((child) => {
            const bits = [
              `${child.childId} [${child.state}]`,
              child.presentation === 'background' ? 'background (dimmed)' : null,
              child.itemId ? `item ${child.itemId}${child.itemKind ? ` (${child.itemKind})` : ''}` : null,
              child.worktreeId !== null ? `worktree ${child.worktreeId}` : null,
              `ownership ${child.ownership}`,
              child.model ? `model ${child.model}` : null,
              child.provider ? `provider ${child.provider}` : null,
              child.reasoning !== null ? `reasoning ${child.reasoning ? 'yes' : 'no'}` : null,
              child.thinking !== null ? `thinking ${child.thinking ? 'yes' : 'no'}` : null,
              child.budget !== null ? `budget ${child.budget}` : null,
            ].filter((bit): bit is string => bit !== null);
            return bits.join(' · ');
          })
        : ['none'],
    evidence: [],
  });
  sections.push({
    key: 'tournament',
    title: 'Tournament',
    present: view.tournament !== null,
    lines:
      view.tournament === null
        ? ['none']
        : [
            `tournament ${view.tournament.id} [${view.tournament.state}] winner ${
              view.tournament.winner ?? '—'
            }`,
            `criteria ${view.tournament.criteria.length}: ${
              view.tournament.criteria.join('; ') || '—'
            }`,
            ...view.tournament.candidates.map((candidate) => {
              const verdict =
                candidate.verification === null
                  ? 'unverified'
                  : candidate.verificationPass === true
                    ? 'pass'
                    : 'fail';
              const review =
                candidate.reviewRank === null
                  ? 'no review'
                  : `${candidate.reviewRank}${
                      candidate.reviewer !== null ? ` (${candidate.reviewer})` : ''
                    }`;
              return `${candidate.winner ? '* ' : ''}${candidate.childId} [${candidate.state}] ${verdict} ${review} cost ${candidate.costMicro} wall ${candidate.wallMs}ms`;
            }),
          ],
    evidence: [],
    actions:
      view.tournament === null
        ? []
        : [
            { key: 'decide', label: 'Decide winner', enabled: view.tournament.canDecide },
            { key: 'abort', label: 'Abort', enabled: view.tournament.open },
          ],
  });
  sections.push({
    key: 'phase',
    title: 'Current phase',
    present: view.phase.length > 0 && view.phase !== 'unknown',
    lines: [`${view.phase} — ${view.state}`],
    evidence: [],
  });
  sections.push({
    key: 'blockers',
    title: 'Blockers',
    present: view.blockers.length > 0,
    lines: view.blockers.length > 0 ? view.blockers.map((blocker) => `! ${blocker}`) : ['none'],
    evidence: [],
  });
  sections.push({
    key: 'verification',
    title: 'Verification',
    present: true,
    lines: [
      `status ${view.verification.status}`,
      // The top-level summary: only a served record that proves the full
      // criterion set with no failed check renders VERIFIED; criteria,
      // checks, review, tree, published commit, remote PR head and cost are
      // appended when the payload serves them.
      ...(view.verification.verified
        ? [
            [
              'VERIFIED',
              `criteria ${view.verification.criteriaPassed}/${view.verification.criteriaTotal}`,
              `checks failed ${view.verification.checksFailed}`,
              view.verification.reviewer !== null
                ? `review ${view.verification.reviewer}`
                : null,
              view.verification.tree !== null
                ? `tree ${digestLabel(view.verification.tree)}`
                : null,
              view.verification.publishedCommit !== null
                ? `commit ${digestLabel(view.verification.publishedCommit)}`
                : null,
              view.verification.remotePrHead !== null
                ? `head ${digestLabel(view.verification.remotePrHead)}`
                : null,
              view.spend !== null
                ? `cost ${microUsdText(BigInt(view.spend.spentCostMicro))}`
                : null,
            ]
              .filter((bit): bit is string => bit !== null)
              .join(' · '),
          ]
        : []),
      `criteria ${view.verification.criteriaPassed}/${view.verification.criteriaTotal} passed`,
      `checks failed ${view.verification.checksFailed}`,
      `owed ${view.verification.owed} · failed checks ${view.verification.failedChecks}`,
    ],
    evidence: [],
  });
  sections.push({
    key: 'evidence',
    title: 'Evidence',
    present: view.evidence.length > 0,
    lines: view.evidence.length > 0 ? view.evidence.map((ref) => ref.label) : ['none'],
    evidence: view.evidence,
  });
  sections.push({
    key: 'spend',
    title: 'Spend',
    present: view.spend !== null,
    lines: view.spend
      ? [
          `tokens ${view.spend.spentTokens ?? '—'}${
            view.spend.maxTokens !== null ? ` / ${view.spend.maxTokens}` : ''
          }`,
          `cost ${microUsdText(BigInt(view.spend.spentCostMicro))}${
            view.spend.maxCostMicro !== null
              ? ` / ${microUsdText(BigInt(view.spend.maxCostMicro))}`
              : ''
          }`,
          `reserved ${microUsdText(BigInt(view.spend.openReservedMicro))}`,
        ]
      : ['none'],
    evidence: [],
  });
  return sections;
}
