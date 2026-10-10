// The durable engineering contract of a task, as the Faktor panels render it
// (`docs/specs/autonomous-engineering.md`, frozen in
// `crates/core/src/contract.rs`).
//
// This module is PURE and dependency-light so scripts/selftest.mjs can render
// every projection from a canned native payload. It owns two jobs:
//
//   1. the TOLERANT additive wire decode of the task DTO's `contract` member
//      (a legacy daemon serves no member => null, never fabricated; a present
//      member is decoded per-entry and malformed entries are disclosed, never
//      silently rendered as valid);
//   2. the bounded view projection the cockpit section and the JetBrains
//      parity model share conceptually: provenance labels, severity/state
//      chips, confidence text and the completion-disclosure decision
//      (`requires_disclosure`).
//
// Nothing here invents data: an unserved member is null, an empty statement
// is dropped from the view, and a malformed severity/state falls back to the
// WEAKEST state (`unknown`) so a corrupt payload can only ever make the UI
// disclose more, never claim more.

import { safeSlice, stripDisplayControls } from './displayText.ts';

/** Mirrors of the daemon's own bounds (<=64 rows, <=512 bytes each). */
export const MAX_CONTRACT_ROWS = 64;
export const MAX_CONTRACT_ROW_BYTES = 512;
export const MAX_CONTRACT_DISCLOSURES = 32;
const MAX_LABEL_CHARS = 240;
const MAX_PROVIDER_CHARS = 64;

export type ContractOriginKind = 'user' | 'policy' | 'native' | 'heuristic' | 'external';

/** Where one contract entry came from; an external provider is named. */
export interface ContractOriginView {
  readonly kind: ContractOriginKind;
  readonly provider: string | null;
}

export type RiskSeverityView = 'low' | 'medium' | 'high' | 'unknown';

export type ClaimStateView =
  | 'known_true'
  | 'supported'
  | 'inferred'
  | 'uncertain'
  | 'unknown'
  | 'disproven';

export type ProofEvidenceKindView =
  | 'compile'
  | 'test_execution'
  | 'contract_check'
  | 'review'
  | 'runtime_trace';

export interface ContractConstraintView {
  readonly id: string;
  readonly statement: string;
  readonly origin: ContractOriginView;
}

export interface ContractInvariantView {
  readonly id: string;
  readonly statement: string;
  readonly origin: ContractOriginView;
}

export interface ContractRiskView {
  readonly id: string;
  readonly description: string;
  readonly severity: RiskSeverityView;
  readonly origin: ContractOriginView;
}

export interface ContractEvidenceRefView {
  readonly kind: ProofEvidenceKindView | null;
  readonly source: string;
  readonly detail: string;
}

export interface ContractObligationView {
  readonly id: string;
  readonly claim: string;
  /** The evidence kind that can discharge the claim, when recognized. */
  readonly evidence: ProofEvidenceKindView | null;
  /** The display label: a recognized kind, an unrecognized raw token, or `unknown`. */
  readonly evidenceLabel: string;
  readonly origin: ContractOriginView;
  readonly state: ClaimStateView;
  /** Basis points (0..=10_000) when the daemon derived one. */
  readonly confidenceBps: number | null;
  readonly evidenceRefs: readonly ContractEvidenceRefView[];
}

/** The task's decoded engineering contract (view form). */
export interface EngineeringContractView {
  readonly constraints: readonly ContractConstraintView[];
  readonly invariants: readonly ContractInvariantView[];
  readonly risks: readonly ContractRiskView[];
  readonly proofObligations: readonly ContractObligationView[];
}

/**
 * The result of the additive decode of the task DTO's `contract` member.
 *   - absent   — the serving daemon exposes no contract section (legacy);
 *   - present  — the member decoded; `droppedRows` names entries whose shape
 *                was malformed (their content is NOT rendered, but the count
 *                is disclosed);
 *   - malformed— the member exists but is not an object at all.
 */
export interface EngineeringContractRead {
  readonly contract: EngineeringContractView | null;
  readonly absent: boolean;
  readonly malformed: string | null;
  readonly droppedRows: number;
}

export function emptyEngineeringContract(): EngineeringContractView {
  return { constraints: [], invariants: [], risks: [], proofObligations: [] };
}

export function engineeringContractIsEmpty(contract: EngineeringContractView): boolean {
  return (
    contract.constraints.length === 0 &&
    contract.invariants.length === 0 &&
    contract.risks.length === 0 &&
    contract.proofObligations.length === 0
  );
}

/** The read of a task DTO with no `contract` member at all (legacy daemon). */
export function absentEngineeringContract(): EngineeringContractRead {
  return { contract: null, absent: true, malformed: null, droppedRows: 0 };
}

// ------------------------------------------------------------- wire decode

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function boundedLabel(value: unknown, max = MAX_LABEL_CHARS): string {
  if (typeof value !== 'string') {
    return '';
  }
  return displayTextOf(value, max);
}

function displayTextOf(value: string, max: number): string {
  const clean = stripDisplayControls(value).trim();
  return clean.length > max ? `${safeSlice(clean, max)}…` : clean;
}

/** The `id` is display-invisible bookkeeping; an absent one is tolerated. */
function idOf(raw: Record<string, unknown>): string {
  const value = raw.id;
  return typeof value === 'string' ? displayTextOf(value, MAX_LABEL_CHARS) : '';
}

function statementOf(raw: Record<string, unknown>, keys: readonly string[]): string {
  for (const key of keys) {
    const value = raw[key];
    if (typeof value === 'string' && value.trim().length > 0) {
      return displayTextOf(value, MAX_CONTRACT_ROW_BYTES);
    }
  }
  return '';
}

/**
 * Decode one provenance value. The frozen serde vocabulary is `"user"`,
 * `"policy"`, `"native"`, `"heuristic"` or `{"external":{"provider":...}}`;
 * anything else is NOT fabricated into a known origin (null => the entry is
 * disclosed as a dropped row).
 */
function originOf(raw: unknown): ContractOriginView | null {
  if (typeof raw === 'string') {
    if (raw === 'user' || raw === 'policy' || raw === 'native' || raw === 'heuristic') {
      return { kind: raw, provider: null };
    }
    return null;
  }
  if (!isPlainObject(raw)) {
    return null;
  }
  const external = raw.external;
  if (isPlainObject(external)) {
    const provider = external.provider;
    if (typeof provider === 'string' && provider.trim().length > 0) {
      return { kind: 'external', provider: displayTextOf(provider, MAX_PROVIDER_CHARS) };
    }
    return null;
  }
  if (typeof external === 'string' && external.trim().length > 0) {
    // Tolerated single-string external form; the provider is still named.
    return { kind: 'external', provider: displayTextOf(external, MAX_PROVIDER_CHARS) };
  }
  return null;
}

/** The weakest severity is `unknown` (disclosure-requiring), never `low`. */
function severityOf(raw: unknown): RiskSeverityView {
  if (raw === 'low' || raw === 'medium' || raw === 'high' || raw === 'unknown') {
    return raw;
  }
  return 'unknown';
}

/** The weakest claim state is `unknown` (disclosure-requiring). */
function stateOf(raw: unknown): ClaimStateView {
  if (
    raw === 'known_true' ||
    raw === 'supported' ||
    raw === 'inferred' ||
    raw === 'uncertain' ||
    raw === 'unknown' ||
    raw === 'disproven'
  ) {
    return raw;
  }
  return 'unknown';
}

function evidenceKindOf(raw: unknown): ProofEvidenceKindView | null {
  if (
    raw === 'compile' ||
    raw === 'test_execution' ||
    raw === 'contract_check' ||
    raw === 'review' ||
    raw === 'runtime_trace'
  ) {
    return raw;
  }
  return null;
}

function confidenceOf(raw: unknown): number | null {
  if (typeof raw !== 'number' || !Number.isInteger(raw) || raw < 0 || raw > 10_000) {
    return null;
  }
  return raw;
}

function evidenceRefsOf(raw: unknown): { refs: ContractEvidenceRefView[]; dropped: number } {
  if (!Array.isArray(raw)) {
    return { refs: [], dropped: 0 };
  }
  const refs: ContractEvidenceRefView[] = [];
  let dropped = 0;
  for (const entry of raw.slice(0, MAX_CONTRACT_ROWS)) {
    if (!isPlainObject(entry)) {
      dropped += 1;
      continue;
    }
    refs.push({
      kind: evidenceKindOf(entry.kind),
      source: boundedLabel(entry.source),
      detail: boundedLabel(entry.detail),
    });
  }
  if (raw.length > MAX_CONTRACT_ROWS) {
    dropped += raw.length - MAX_CONTRACT_ROWS;
  }
  return { refs, dropped };
}

function decodeConstraints(raw: unknown): { rows: ContractConstraintView[]; dropped: number } {
  return decodeStatementRows(raw, (id, statement, origin) => ({ id, statement, origin }));
}

function decodeInvariants(raw: unknown): { rows: ContractInvariantView[]; dropped: number } {
  return decodeStatementRows(raw, (id, statement, origin) => ({ id, statement, origin }));
}

function decodeStatementRows<T>(
  raw: unknown,
  build: (id: string, statement: string, origin: ContractOriginView) => T,
): { rows: T[]; dropped: number } {
  if (!Array.isArray(raw)) {
    return { rows: [], dropped: 0 };
  }
  const rows: T[] = [];
  let dropped = 0;
  for (const entry of raw.slice(0, MAX_CONTRACT_ROWS)) {
    if (!isPlainObject(entry)) {
      dropped += 1;
      continue;
    }
    const statement = statementOf(entry, ['statement', 'text', 'description']);
    const origin = originOf(entry.origin);
    if (statement.length === 0 || origin === null) {
      dropped += 1;
      continue;
    }
    rows.push(build(idOf(entry), statement, origin));
  }
  if (raw.length > MAX_CONTRACT_ROWS) {
    dropped += raw.length - MAX_CONTRACT_ROWS;
  }
  return { rows, dropped };
}

function decodeRisks(raw: unknown): { rows: ContractRiskView[]; dropped: number } {
  if (!Array.isArray(raw)) {
    return { rows: [], dropped: 0 };
  }
  const rows: ContractRiskView[] = [];
  let dropped = 0;
  for (const entry of raw.slice(0, MAX_CONTRACT_ROWS)) {
    if (!isPlainObject(entry)) {
      dropped += 1;
      continue;
    }
    const description = statementOf(entry, ['description', 'statement', 'text']);
    const origin = originOf(entry.origin);
    if (description.length === 0 || origin === null) {
      dropped += 1;
      continue;
    }
    rows.push({
      id: idOf(entry),
      description,
      // A malformed severity lands on `unknown`, which forces disclosure.
      severity: severityOf(entry.severity),
      origin,
    });
  }
  if (raw.length > MAX_CONTRACT_ROWS) {
    dropped += raw.length - MAX_CONTRACT_ROWS;
  }
  return { rows, dropped };
}

function decodeObligations(raw: unknown): { rows: ContractObligationView[]; dropped: number } {
  if (!Array.isArray(raw)) {
    return { rows: [], dropped: 0 };
  }
  const rows: ContractObligationView[] = [];
  let dropped = 0;
  for (const entry of raw.slice(0, MAX_CONTRACT_ROWS)) {
    if (!isPlainObject(entry)) {
      dropped += 1;
      continue;
    }
    const claim = statementOf(entry, ['claim', 'statement', 'text']);
    const origin = originOf(entry.origin);
    if (claim.length === 0 || origin === null) {
      dropped += 1;
      continue;
    }
    const evidence = evidenceKindOf(entry.evidence);
    const evidenceRaw =
      typeof entry.evidence === 'string' && entry.evidence.trim().length > 0
        ? displayTextOf(entry.evidence, 64)
        : 'unknown';
    const { refs, dropped: refsDropped } = evidenceRefsOf(entry.evidence_refs);
    dropped += refsDropped;
    rows.push({
      id: idOf(entry),
      claim,
      evidence,
      evidenceLabel: evidence === null ? evidenceRaw : evidenceKindLabel(evidence),
      origin,
      // A malformed state lands on `unknown`, which forces disclosure.
      state: stateOf(entry.state),
      confidenceBps: confidenceOf(entry.confidence_bps),
      evidenceRefs: refs,
    });
  }
  if (raw.length > MAX_CONTRACT_ROWS) {
    dropped += raw.length - MAX_CONTRACT_ROWS;
  }
  return { rows, dropped };
}

/**
 * Additive decode of one task DTO's `contract` member. Absent/null => the
 * legacy read (no section). A present non-object is an explicit malformed
 * read. Entries decode independently; malformed entries are counted and
 * disclosed, never rendered as claims.
 */
export function readEngineeringContract(raw: unknown): EngineeringContractRead {
  if (raw === undefined || raw === null) {
    return absentEngineeringContract();
  }
  if (!isPlainObject(raw)) {
    return {
      contract: null,
      absent: false,
      malformed: 'the contract member is not an object',
      droppedRows: 0,
    };
  }
  let droppedRows = 0;
  const constraints = decodeConstraints(raw.constraints);
  const invariants = decodeInvariants(raw.invariants);
  const risks = decodeRisks(raw.risks);
  const obligations = decodeObligations(raw.proof_obligations);
  droppedRows +=
    constraints.dropped + invariants.dropped + risks.dropped + obligations.dropped;
  return {
    contract: {
      constraints: constraints.rows,
      invariants: invariants.rows,
      risks: risks.rows,
      proofObligations: obligations.rows,
    },
    absent: false,
    malformed: null,
    droppedRows,
  };
}

// ------------------------------------------------------------ presentation

export function contractOriginLabel(origin: ContractOriginView): string {
  return origin.kind === 'external' ? origin.provider ?? 'external' : origin.kind;
}

export function claimStateLabel(state: ClaimStateView): string {
  return state.replace(/_/g, ' ');
}

export function riskSeverityLabel(severity: RiskSeverityView): string {
  return severity;
}

export function evidenceKindLabel(kind: ProofEvidenceKindView): string {
  return kind.replace(/_/g, ' ');
}

/** `91%` for 9100 bps; null when the daemon derived no confidence. */
export function confidenceLabel(bps: number | null): string | null {
  if (bps === null) {
    return null;
  }
  return `${Math.round(bps / 100)}%`;
}

/** `supported · 91%` (or just the state when no confidence was derived). */
export function obligationStateText(obligation: ContractObligationView): string {
  const confidence = confidenceLabel(obligation.confidenceBps);
  return confidence === null
    ? claimStateLabel(obligation.state)
    : `${claimStateLabel(obligation.state)} · ${confidence}`;
}

export function riskRequiresDisclosure(severity: RiskSeverityView): boolean {
  return severity === 'high' || severity === 'unknown';
}

export function stateRequiresDisclosure(state: ClaimStateView): boolean {
  return !(state === 'known_true' || state === 'supported');
}

/** TRUE when any high/unknown risk or unsatisfied obligation must be disclosed. */
export function contractNeedsDisclosure(contract: EngineeringContractView): boolean {
  return (
    contract.risks.some((risk) => riskRequiresDisclosure(risk.severity)) ||
    contract.proofObligations.some((obligation) => stateRequiresDisclosure(obligation.state))
  );
}

/** One completion-disclosure row; the state token mirrors the core's own. */
export interface ContractDisclosureLine {
  readonly state: string;
  readonly summary: string;
}

/**
 * The disclosure rows: every high/unknown risk (`risk:<severity>`) and every
 * obligation whose state is not satisfied. Deterministic (risks then
 * obligations, source order) and bounded.
 */
export function contractDisclosureLines(
  contract: EngineeringContractView,
): ContractDisclosureLine[] {
  const lines: ContractDisclosureLine[] = [];
  for (const risk of contract.risks) {
    if (riskRequiresDisclosure(risk.severity)) {
      lines.push({ state: `risk:${risk.severity}`, summary: risk.description });
    }
  }
  for (const obligation of contract.proofObligations) {
    if (stateRequiresDisclosure(obligation.state)) {
      lines.push({ state: obligation.state, summary: obligation.claim });
    }
  }
  return lines.slice(0, MAX_CONTRACT_DISCLOSURES);
}
