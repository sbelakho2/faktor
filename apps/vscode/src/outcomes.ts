// I07: the Engineering Outcome Ledger read surface (docs/outcome-ledger.md).
//
// This module owns the ONE host-side projection of `GET /native/outcomes`:
// the strict generated DTO is validated by the native client, and every
// rendered value here is derived (or explicitly marked unmeasured) from the
// served payload. Dependency-free (no vscode import) so
// scripts/selftest.mjs drives every formatting and honesty rule directly.
//
// Honesty rules encoded here:
//   - an empty verified denominator renders the exact `No verified changes
//     yet` copy (the served numeric zero stays in the data, but is never
//     called a free run);
//   - `avoidedHostedMicro` 0 is `not measured` (the runtime has no
//     counterfactual recorder), never a savings claim; nonzero shows the
//     exact served value;
//   - the Frontier Escalation Rate at zero is `not measured` (no
//     frontier-tier authority records escalations), never `0%`;
//   - every other empty denominator renders `—`, never `0`;
//   - rates render exactly from the served floor ppm (`86.4%`, never
//     rounded up, never scientific); money renders through money.ts exact
//     decimal strings (no float, huge micro values stay exact), with the
//     exact micro string in the detail line.

import {
  validateProtocolOutcomesPage,
  type ProtocolJson,
  type ProtocolOutcomeCursor,
  type ProtocolOutcomeMetrics,
  type ProtocolOutcomesPage,
  type ProtocolRunOutcome,
} from './generated/protocolDto.ts';
import { microFromDecimal, microToString, microUsdText } from './money.ts';

/** One bounded newest-first outcomes page per read (the daemon cap is 200). */
export const OUTCOMES_PAGE_LIMIT = 50;
/** Bounded in-memory run list across older-page reads. */
export const MAX_OUTCOME_RUNS = 200;

export const NO_VERIFIED_CHANGES = 'No verified changes yet';
export const NOT_MEASURED = 'not measured';
export const UNAVAILABLE_VALUE = '—';
export const TRUNCATED_SAMPLE_NOTE = 'Truncated sample';
/** The distinct read-failure state (T29): no monetary value is rendered. */
export const COST_UNAVAILABLE_LABEL = 'Cost unavailable';
export const COST_UNAVAILABLE_COPY = 'The current authority has not supplied a cost value.';

export interface OutcomeMetricView {
  readonly key: string;
  /** The exact ledger metric name. */
  readonly label: string;
  readonly group: 'cost' | 'rate' | 'local' | 'flow' | 'quality';
  readonly cohort:
    | 'local-only'
    | 'local-final'
    | 'local-first'
    | 'local-to-frontier'
    | 'frontier-only'
    | null;
  readonly value: string;
  /** The served counts/denominators behind the value. */
  readonly detail: string;
  /** The short definition behind the `Explain metric` disclosure. */
  readonly definition: string;
  /** TRUE when the value is honest absence (`—` / `not measured`). */
  readonly unmeasured: boolean;
}

export interface OutcomeRunView {
  readonly runId: string;
  readonly taskClass: string;
  readonly riskClass: string;
  readonly verdict: string;
  readonly accepted: boolean;
  readonly integrated: boolean;
  readonly verified: boolean;
  readonly firstPass: boolean;
  readonly repairAttempts: number;
  readonly completedText: string;
  readonly modelSpendUsd: string;
  readonly modelSpendExact: string;
  readonly verificationSpendExact: string;
  readonly reworkSpendExact: string;
  readonly avoidedHostedExact: string;
  readonly wallText: string;
  readonly changeFiles: number;
  readonly addedLines: number;
  readonly removedLines: number;
  readonly humanInterventions: number;
  readonly rollbackEvents: number;
  readonly recoveryEvents: number;
  readonly frontierEscalations: number;
  readonly steps: string;
  readonly sessionId: number;
  readonly taskId: number;
}

export interface OutcomesView {
  readonly state: 'ok' | 'unavailable';
  /** The explicit refusal reason of a failed read; null while `ok`. */
  readonly reason: string | null;
  readonly reasonCode: string | null;
  readonly scanned: number;
  readonly truncated: boolean;
  readonly metrics: readonly OutcomeMetricView[];
  /** The one-line distinction between the two cost metrics. */
  readonly costNote: string;
  readonly runs: readonly OutcomeRunView[];
  readonly nextCursor: { readonly completedAtMs: number; readonly runId: string } | null;
  readonly pageCount: number;
}

const COST_NOTE =
  'Successful-Run Cost averages only the runs that verified; ' +
  'Verified Cost per Successful Change (VCSC) charges every attempted run ' +
  '(failed and cancelled included) to the changes that landed.';

/**
 * Exact ppm -> percent text (floor to the ppm the server served; floor ppm
 * means the true rate is AT LEAST the displayed value). Never rounds up,
 * never emits scientific notation, trims trailing zeros.
 */
export function formatPpm(ppm: number): string {
  if (!Number.isFinite(ppm) || ppm < 0) {
    return UNAVAILABLE_VALUE;
  }
  const whole = Math.floor(ppm / 10_000);
  const fraction = Math.floor(ppm % 10_000);
  if (fraction === 0) {
    return `${whole}%`;
  }
  const digits = String(fraction).padStart(4, '0').replace(/0+$/, '');
  return `${whole}.${digits}%`;
}

/** Exact USD display of one served decimal micro string (never a float). */
export function usdTextOfMicro(raw: string): string | null {
  const value = microFromDecimal(raw);
  return value === null ? null : `${microUsdText(value)} USD`;
}

/** The exact micro-USD text of one served decimal micro string. */
export function microExactTextOf(raw: string): string | null {
  const value = microFromDecimal(raw);
  return value === null ? null : `${microToString(value)} µ$`;
}

function moneyValue(raw: string): string {
  return usdTextOfMicro(raw) ?? UNAVAILABLE_VALUE;
}

function exactMoney(raw: string): string {
  return microExactTextOf(raw) ?? UNAVAILABLE_VALUE;
}

function count(value: number): string {
  return Number.isSafeInteger(value) ? String(value) : UNAVAILABLE_VALUE;
}

function metric(input: {
  readonly key: string;
  readonly label: string;
  readonly group: OutcomeMetricView['group'];
  readonly cohort?: OutcomeMetricView['cohort'];
  readonly value: string;
  readonly detail: string;
  readonly definition: string;
  readonly unmeasured?: boolean;
}): OutcomeMetricView {
  return {
    key: input.key,
    label: input.label,
    group: input.group,
    cohort: input.cohort ?? null,
    value: input.value,
    detail: input.detail,
    definition: input.definition,
    unmeasured: input.unmeasured === true,
  };
}

/**
 * Build the bounded metrics rows from one served fold. The served zero
 * values stay in `detail` counts; only the DISPLAYED value follows the
 * honesty rules above.
 */
export function outcomeMetricViews(m: ProtocolOutcomeMetrics): OutcomeMetricView[] {
  const verifiedChanges = m.verifiedChanges;
  const noVerified = !Number.isSafeInteger(verifiedChanges) || verifiedChanges === 0;
  const runs = m.runs;
  const noRuns = !Number.isSafeInteger(runs) || runs === 0;
  const totalSpend = microFromDecimal(m.totalSpendMicro);
  const noTotalSpend = totalSpend === null || totalSpend === 0n;
  const avoided = microFromDecimal(m.avoidedHostedInferenceMicro);
  const avoidedRecorded = avoided !== null && avoided > 0n;
  const rows: OutcomeMetricView[] = [];

  rows.push(
    metric({
      key: 'successfulRunCostMicro',
      label: 'Successful-Run Cost',
      group: 'cost',
      value: noVerified ? NO_VERIFIED_CHANGES : moneyValue(m.successfulRunCostMicro),
      detail: noVerified
        ? `${exactMoney(m.totalVerifiedSpendMicro)} verified-run spend · 0 verified changes`
        : `${exactMoney(m.totalVerifiedSpendMicro)} verified-run spend ÷ ${count(verifiedChanges)} verified changes`,
      definition:
        'Model + verification spend over the successful (verified) runs divided by verified ' +
        'changes, rounded up — the average spend of a run that DID succeed.',
      // The exact copy is honest absence, not a measured zero.
      unmeasured: noVerified,
    }),
    metric({
      key: 'verifiedCostPerSuccessfulChangeMicro',
      label: 'Verified Cost per Successful Change (VCSC)',
      group: 'cost',
      value: noVerified ? NO_VERIFIED_CHANGES : moneyValue(m.verifiedCostPerSuccessfulChangeMicro),
      detail: noVerified
        ? `${exactMoney(m.totalSpendMicro)} all-attempt spend · 0 verified changes`
        : `${exactMoney(m.totalSpendMicro)} all-attempt spend ÷ ${count(verifiedChanges)} verified changes`,
      definition:
        'TOTAL eligible-cohort spend over ALL attempted work (failed and cancelled runs ' +
        'included) divided by verified changes, rounded up. Distinct from Successful-Run Cost.',
      unmeasured: noVerified,
    }),
    metric({
      key: 'verifiedFirstPassPpm',
      label: 'Verified First-Pass Rate',
      group: 'rate',
      value: noVerified ? NO_VERIFIED_CHANGES : formatPpm(m.verifiedFirstPassPpm),
      detail: `${count(m.verifiedFirstPassChanges)}/${count(verifiedChanges)} verified changes`,
      definition:
        'First-attempt verified changes ÷ verified changes (floor ppm: the true rate is at least this).',
      unmeasured: noVerified,
    }),
    metric({
      key: 'reworkCostRatioPpm',
      label: 'Rework Cost Ratio',
      group: 'rate',
      value: noTotalSpend ? UNAVAILABLE_VALUE : formatPpm(m.reworkCostRatioPpm),
      detail: noTotalSpend
        ? 'no model + verification spend recorded; zero denominator'
        : `${exactMoney(m.totalReworkSpendMicro)} rework ÷ ${exactMoney(m.totalSpendMicro)} (model + verification) spend`,
      definition: 'Rework spend ÷ (model + verification) spend over all runs.',
      unmeasured: noTotalSpend,
    }),
  );

  // The four local cohorts stay visually distinct (group local + cohort tag).
  rows.push(
    metric({
      key: 'localOnlyVerifiedPpm',
      label: 'Local-Only Verified Rate',
      group: 'local',
      cohort: 'local-only',
      value:
        Number.isSafeInteger(m.localOnlyRuns) && m.localOnlyRuns > 0
          ? formatPpm(m.localOnlyVerifiedPpm)
          : UNAVAILABLE_VALUE,
      detail:
        Number.isSafeInteger(m.localOnlyRuns) && m.localOnlyRuns > 0
          ? `${count(m.localOnlyVerifiedChanges)}/${count(m.localOnlyRuns)} local-only runs`
          : 'no local-only runs recorded',
      definition:
        'Verified local-only runs ÷ local-only runs (runs whose model sequence is entirely local).',
      unmeasured: !(Number.isSafeInteger(m.localOnlyRuns) && m.localOnlyRuns > 0),
    }),
    metric({
      key: 'localFinalVerifiedPpm',
      label: 'Local-Final Verified Rate',
      group: 'local',
      cohort: 'local-final',
      value:
        Number.isSafeInteger(m.localFinalRuns) && m.localFinalRuns > 0
          ? formatPpm(m.localFinalVerifiedPpm)
          : UNAVAILABLE_VALUE,
      detail:
        Number.isSafeInteger(m.localFinalRuns) && m.localFinalRuns > 0
          ? `${count(m.localFinalVerifiedChanges)}/${count(m.localFinalRuns)} runs ending on a local step`
          : 'no local-final runs recorded',
      definition:
        'Verified local-final runs ÷ runs whose FINAL model step is local.',
      unmeasured: !(Number.isSafeInteger(m.localFinalRuns) && m.localFinalRuns > 0),
    }),
    metric({
      key: 'localFirstSuccessPpm',
      label: 'Local-First Success Rate',
      group: 'local',
      cohort: 'local-first',
      value:
        Number.isSafeInteger(m.localFirstRuns) && m.localFirstRuns > 0
          ? formatPpm(m.localFirstSuccessPpm)
          : UNAVAILABLE_VALUE,
      detail:
        Number.isSafeInteger(m.localFirstRuns) && m.localFirstRuns > 0
          ? `${count(m.localFirstSuccessChanges)}/${count(m.localFirstRuns)} runs starting on a local step`
          : 'no local-first runs recorded',
      definition:
        'Local-first runs that verified on their FIRST attempt ÷ runs whose first model step is local.',
      unmeasured: !(Number.isSafeInteger(m.localFirstRuns) && m.localFirstRuns > 0),
    }),
    metric({
      key: 'localToFrontierFallbackPpm',
      label: 'Local-to-Frontier Fallback Rate',
      group: 'local',
      cohort: 'local-to-frontier',
      value:
        Number.isSafeInteger(m.localFirstRuns) && m.localFirstRuns > 0
          ? formatPpm(m.localToFrontierFallbackPpm)
          : UNAVAILABLE_VALUE,
      detail:
        Number.isSafeInteger(m.localFirstRuns) && m.localFirstRuns > 0
          ? `${count(m.localToFrontierFallbackRuns)}/${count(m.localFirstRuns)} runs starting on a local step`
          : 'no local-first runs recorded',
      definition:
        'Runs that started local and ENDED hosted ÷ runs whose first step is local. A ' +
        'local-then-hosted repair is a FALLBACK, never a local substitution.',
      unmeasured: !(Number.isSafeInteger(m.localFirstRuns) && m.localFirstRuns > 0),
    }),
    metric({
      key: 'frontierOnlyPpm',
      label: 'Frontier-Only Rate',
      group: 'local',
      cohort: 'frontier-only',
      value: noRuns ? UNAVAILABLE_VALUE : formatPpm(m.frontierOnlyPpm),
      detail: noRuns
        ? 'no runs recorded'
        : `${count(m.frontierOnlyRuns)}/${count(runs)} runs with no local step`,
      definition: 'Runs with no local step at all ÷ all runs.',
      unmeasured: noRuns,
    }),
  );

  rows.push(
    metric({
      key: 'avoidedHostedInferenceMicro',
      label: 'Avoided Hosted Inference',
      group: 'cost',
      value: avoidedRecorded
        ? `${moneyValue(m.avoidedHostedInferenceMicro)} (${exactMoney(m.avoidedHostedInferenceMicro)})`
        : NOT_MEASURED,
      detail: avoidedRecorded
        ? 'recorded counterfactual hosted inference avoided by local substitution'
        : 'no counterfactual recorder has supplied an avoided reduction; never a savings claim',
      definition:
        'Sum of recorded avoided_hosted_micro over all runs. 0 means unmeasured, never "no savings".',
      unmeasured: !avoidedRecorded,
    }),
    metric({
      key: 'meanTimeToVerifiedChangeMs',
      label: 'Mean Time to Verified Change',
      group: 'quality',
      value: noVerified ? NO_VERIFIED_CHANGES : `${count(m.meanTimeToVerifiedChangeMs)} ms`,
      detail: noVerified
        ? `${count(m.totalWallTimeMs)} ms wall time · 0 verified changes`
        : `${count(m.totalWallTimeMs)} ms wall time ÷ ${count(verifiedChanges)} verified changes`,
      definition: 'Wall time over verified changes ÷ verified changes, rounded up.',
      unmeasured: noVerified,
    }),
    metric({
      key: 'humanInterventionPpm',
      label: 'Human Intervention Rate',
      group: 'flow',
      value: noRuns ? UNAVAILABLE_VALUE : formatPpm(m.humanInterventionPpm),
      detail: noRuns
        ? 'no runs recorded'
        : `${count(m.humanInterventionRuns)}/${count(runs)} runs with at least one intervention`,
      definition: 'Runs with at least one human intervention ÷ all runs.',
      unmeasured: noRuns,
    }),
    metric({
      key: 'regressionRollbackPpm',
      label: 'Regression/Rollback Rate',
      group: 'flow',
      value: noRuns ? UNAVAILABLE_VALUE : formatPpm(m.regressionRollbackPpm),
      detail: noRuns
        ? 'no runs recorded'
        : `${count(m.rollbackRuns)}/${count(runs)} runs with at least one rollback event`,
      definition: 'Runs with at least one rollback event ÷ all runs.',
      unmeasured: noRuns,
    }),
    metric({
      key: 'proofCoveragePpm',
      label: 'Proof Coverage',
      group: 'quality',
      value:
        Number.isSafeInteger(m.checksRequired) && m.checksRequired > 0
          ? formatPpm(m.proofCoveragePpm)
          : UNAVAILABLE_VALUE,
      detail:
        Number.isSafeInteger(m.checksRequired) && m.checksRequired > 0
          ? `${count(m.checksPassed)}/${count(m.checksRequired)} required checks`
          : 'no required checks recorded',
      definition:
        'Passed checks ÷ required checks over runs that required any (clamped to 100%).',
      unmeasured: !(Number.isSafeInteger(m.checksRequired) && m.checksRequired > 0),
    }),
    metric({
      key: 'frontierEscalationPpm',
      label: 'Frontier Escalation Rate',
      group: 'flow',
      value:
        Number.isSafeInteger(m.frontierEscalationRuns) && m.frontierEscalationRuns > 0 && !noRuns
          ? formatPpm(m.frontierEscalationPpm)
          : NOT_MEASURED,
      detail:
        Number.isSafeInteger(m.frontierEscalationRuns) && m.frontierEscalationRuns > 0 && !noRuns
          ? `${count(m.frontierEscalationRuns)}/${count(runs)} runs with a recorded frontier escalation`
          : 'no frontier-tier authority records escalations; zero is unmeasured',
      definition:
        'Runs with at least one recorded frontier escalation ÷ all runs. No frontier-tier ' +
        'authority records escalations today, so zero renders not measured.',
      unmeasured: !(
        Number.isSafeInteger(m.frontierEscalationRuns) && m.frontierEscalationRuns > 0 && !noRuns
      ),
    }),
  );
  return rows;
}

function stepLabel(step: ProtocolRunOutcome['modelSequence'][number]): string {
  const provider = step.provider.trim();
  const model = step.model.trim();
  const name = model.length > 0 ? (provider.length > 0 ? `${provider}/${model}` : model) : provider;
  return `${step.local ? 'local' : 'hosted'}:${name.length > 0 ? name : 'unnamed'}`;
}

function completedText(ms: number): string {
  if (!Number.isFinite(ms)) {
    return UNAVAILABLE_VALUE;
  }
  const date = new Date(ms);
  if (!Number.isFinite(date.getTime())) {
    return UNAVAILABLE_VALUE;
  }
  return date.toISOString().slice(0, 19).replace('T', ' ') + 'Z';
}

/** One served run record as the bounded panel row (run id stays advanced). */
export function outcomeRunView(run: ProtocolRunOutcome): OutcomeRunView {
  const verdict = run.verified
    ? 'verified change'
    : run.accepted && run.integrated
      ? 'integrated; verification unavailable'
      : run.accepted
        ? 'accepted; not integrated'
        : 'not accepted';
  const steps = run.modelSequence.map(stepLabel).join(' · ');
  const spend = microFromDecimal(run.modelSpendMicro);
  return {
    runId: run.runId,
    taskClass: run.taskClass,
    riskClass: run.riskClass,
    verdict,
    accepted: run.accepted,
    integrated: run.integrated,
    verified: run.verified,
    firstPass: run.verifiedFirstAttempt,
    repairAttempts: run.repairAttempts,
    completedText: completedText(run.completedAtMs),
    modelSpendUsd: spend === null ? UNAVAILABLE_VALUE : `${microUsdText(spend)} USD`,
    modelSpendExact: exactMoney(run.modelSpendMicro),
    verificationSpendExact: exactMoney(run.verificationSpendMicro),
    reworkSpendExact: exactMoney(run.reworkSpendMicro),
    avoidedHostedExact: exactMoney(run.avoidedHostedMicro),
    wallText: Number.isSafeInteger(run.wallTimeMs) ? `${run.wallTimeMs} ms` : UNAVAILABLE_VALUE,
    changeFiles: run.changeScale.files,
    addedLines: run.changeScale.addedLines,
    removedLines: run.changeScale.removedLines,
    humanInterventions: run.humanInterventions,
    rollbackEvents: run.rollbackEvents,
    recoveryEvents: run.recoveryEvents,
    frontierEscalations: run.frontierEscalations,
    steps: steps.length > 0 ? steps : 'no model steps recorded',
    sessionId: run.sessionId,
    taskId: run.taskId,
  };
}

function cursorView(cursor: ProtocolOutcomeCursor | null): OutcomesView['nextCursor'] {
  if (cursor === null) {
    return null;
  }
  return { completedAtMs: cursor.completedAtMs, runId: cursor.runId };
}

/** Build the view from one freshly read page (newest-first). */
export function buildOutcomesView(page: ProtocolOutcomesPage): OutcomesView {
  const metrics = page.metrics;
  const runs = page.runs.slice(0, MAX_OUTCOME_RUNS).map(outcomeRunView);
  return {
    state: 'ok',
    reason: null,
    reasonCode: null,
    scanned: metrics.scanned,
    truncated: metrics.truncated === true,
    metrics: outcomeMetricViews(metrics),
    costNote: COST_NOTE,
    runs,
    nextCursor: cursorView(page.nextCursor),
    pageCount: 1,
  };
}

/**
 * Append one older page to the loaded view. A refresh (cursor null) callers
 * use `buildOutcomesView`; this path preserves the already loaded newest
 * rows, dedupes by run id, and re-bounds the list.
 */
export function mergeOutcomesPage(
  previous: OutcomesView | null,
  page: ProtocolOutcomesPage,
): OutcomesView {
  if (previous === null || previous.state !== 'ok') {
    return buildOutcomesView(page);
  }
  const seen = new Set(previous.runs.map((run) => run.runId));
  const appended: OutcomeRunView[] = [];
  for (const run of page.runs) {
    if (seen.has(run.runId)) {
      continue;
    }
    seen.add(run.runId);
    appended.push(outcomeRunView(run));
  }
  const runs = [...previous.runs, ...appended].slice(0, MAX_OUTCOME_RUNS);
  return {
    state: 'ok',
    reason: null,
    reasonCode: null,
    scanned: page.metrics.scanned,
    truncated: page.metrics.truncated === true,
    metrics: outcomeMetricViews(page.metrics),
    costNote: COST_NOTE,
    runs,
    nextCursor: cursorView(page.nextCursor),
    pageCount: previous.pageCount + 1,
  };
}

/** The explicit read-failure state (T29): no monetary value is rendered. */
export function unavailableOutcomesView(reason: string, reasonCode: string | null): OutcomesView {
  return {
    state: 'unavailable',
    reason: reason.length > 240 ? `${reason.slice(0, 240)}…` : reason,
    reasonCode,
    scanned: 0,
    truncated: false,
    metrics: [],
    costNote: COST_NOTE,
    runs: [],
    nextCursor: null,
    pageCount: 0,
  };
}

/**
 * Strict parse of one `readOutcomes` request: both cursor members or
 * neither. A half-given or malformed cursor is a typed reason (the host
 * refuses the read loudly instead of silently re-reading the top page).
 */
export function parseOutcomesReadRequest(
  rawMs: unknown,
  rawRun: unknown,
): { readonly beforeMs: number | null; readonly beforeRun: string | null } | { readonly reason: string } {
  const hasMs = rawMs !== undefined && rawMs !== null;
  const hasRun = rawRun !== undefined && rawRun !== null;
  if (!hasMs && !hasRun) {
    return { beforeMs: null, beforeRun: null };
  }
  if (hasMs !== hasRun) {
    return { reason: 'the outcomes cursor requires both beforeMs and beforeRun' };
  }
  if (typeof rawMs !== 'number' || !Number.isSafeInteger(rawMs) || rawMs < 0) {
    return { reason: 'outcomes cursor beforeMs must be a non-negative integer timestamp' };
  }
  if (typeof rawRun !== 'string' || rawRun.trim().length === 0 || rawRun.length > 200) {
    return { reason: 'outcomes cursor beforeRun must be a bounded non-empty run id' };
  }
  return { beforeMs: rawMs, beforeRun: rawRun.trim() };
}

/**
 * The strict parse of the raw `/native/outcomes` response body: the
 * generated DTO validates every known member exactly (unknown additive
 * fields are ignored) and a malformed payload is a typed protocol refusal,
 * never a partially rendered page.
 */
export function parseOutcomesPage(json: unknown): ProtocolOutcomesPage {
  return validateProtocolOutcomesPage(json as ProtocolJson, 'GET /native/outcomes');
}
