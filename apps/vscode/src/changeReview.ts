// I03: the Changes surface and the native immutable-basis review adapter.
//
// Two responsibilities, both dependency-free (no vscode import) so
// scripts/selftest.mjs drives every derivation and refusal directly:
//
//   1. derive the bounded change rows from SERVED data only — the
//      projection's `filesChanged`, the run's verification records and the
//      strict proof summary. A ref is attached only through a served
//      relationship (a verification record listing the path, a criterion
//      whose typed binding names the path, or the criterion's own typed
//      `evidence:` refs); when nothing is served the row carries no refs and
//      nothing is guessed.
//   2. resolve the IMMUTABLE BASIS of one selected change in the exact
//      documented order: (a) the run's served `candidateProof.sourceDiffEvidence`
//      evidence id, (b) else an attached evidence ref of the path, (c) else
//      an explicit `Diff unavailable` state. The basis is served evidence
//      BYTES plus its sourceRevision — never a live workspace file and never
//      a two-sided workspace diff (no server route exists for one).
//
// The virtual documents are read-only text documents under the dedicated
// `faktor-evidence` scheme. The extension host registers the provider; the
// content is loaded through this module's `loadEvidenceDocument`, which
// refuses typed on an unknown/malformed id, a cross-session id, a malformed
// payload and an over-bound retrieval. No workspace file is ever read.

import type {
  NativeEvidence,
  NativeEvidenceRetrieval,
  NativeTaskProof,
  NativeTaskVerification,
} from './nativeClient.ts';

export const FAKTOR_EVIDENCE_SCHEME = 'faktor-evidence';

/** Exact first-line marker of a policy-truncated served artifact. */
export const TRUNCATED_EVIDENCE_HEADER =
  '[truncated by policy: only the served prefix is shown]';

/** The exact in-view state copy when no immutable basis is served. */
export const DIFF_UNAVAILABLE_LABEL = 'Diff unavailable';
export const DIFF_UNAVAILABLE_COPY =
  'The runtime does not serve an immutable candidate content read for this change.';

export const MAX_CHANGE_PATHS = 200;
export const MAX_CHANGE_REFS = 16;
export const MAX_PATH_CHARS = 1024;
export const MAX_DOCUMENT_TITLE_CHARS = 300;
export const MAX_CANDIDATE_DISPLAY_CHARS = 120;

export type ChangeReviewErrorCode =
  | 'invalid_uri'
  | 'unknown_evidence'
  | 'cross_session'
  | 'malformed_payload'
  | 'oversized'
  | 'fetch_failed';

/** Typed refusal of the evidence-document adapter (never a silent failure). */
export class ChangeReviewError extends Error {
  readonly code: ChangeReviewErrorCode;

  constructor(code: ChangeReviewErrorCode, message: string) {
    super(message);
    this.name = 'ChangeReviewError';
    this.code = code;
  }
}

// ---------------------------------------------------------- change rows

export interface ChangeRefSummary {
  readonly kind: 'criterion' | 'evidence' | 'record';
  readonly label: string;
  /** Set on criterion refs: the served criterion key ("Open claim"). */
  readonly criterionKey: string | null;
  /** Set on evidence refs: the served numeric evidence id. */
  readonly evidenceId: number | null;
}

export interface ChangeRowSummary {
  readonly path: string;
  /**
   * The served change kind when the payload carries one. The current server
   * routes serve none, so this is honestly null (the row renders no kind).
   */
  readonly changeKind: string | null;
  readonly refs: readonly ChangeRefSummary[];
}

function boundedPath(value: string): string | null {
  const trimmed = value.trim();
  if (trimmed.length === 0 || trimmed.length > MAX_PATH_CHARS) {
    return null;
  }
  for (let i = 0; i < trimmed.length; i += 1) {
    const code = trimmed.charCodeAt(i);
    if (code < 0x20 || code === 0x7f) {
      return null;
    }
  }
  return trimmed;
}

/** Every numeric `evidence:<n>` / `evidence#<n>` ref inside one served text. */
export function evidenceIdsIn(raw: string | null | undefined): number[] {
  if (typeof raw !== 'string' || raw.length === 0) {
    return [];
  }
  const out: number[] = [];
  const pattern = /(?:^|[^\w])evidence[:#/](\d+)(?![\d])/g;
  let match: RegExpExecArray | null = null;
  while ((match = pattern.exec(raw)) !== null) {
    const value = Number(match[1]);
    if (Number.isSafeInteger(value) && value >= 1 && value <= Number.MAX_SAFE_INTEGER) {
      out.push(value);
    }
    if (out.length >= MAX_CHANGE_REFS) {
      break;
    }
  }
  return out;
}

/** TRUE when the served evidence text carries the typed `file:<path>` ref. */
function evidenceNamesFile(raw: string | null | undefined, path: string): boolean {
  if (typeof raw !== 'string' || raw.length === 0) {
    return false;
  }
  for (const segment of raw.split(';')) {
    const trimmed = segment.trim();
    if (trimmed === `file:${path}`) {
      return true;
    }
  }
  return false;
}

/** The served `path`/`evidenceId` members of one proof summary binding Json. */
function proofBindingPath(binding: unknown): string | null {
  if (typeof binding !== 'object' || binding === null || Array.isArray(binding)) {
    return null;
  }
  const record = binding as Record<string, unknown>;
  for (const key of ['path', 'file', 'fileState']) {
    const value = record[key];
    if (typeof value === 'string' && value.trim().length > 0) {
      return value.trim();
    }
  }
  return null;
}

/**
 * Derive the bounded change rows from served data. `filesChanged` is the
 * authoritative path list (the projection's own served member); refs are
 * attached ONLY through served relationships. Duplicate paths collapse to
 * one row (order preserved); over-bound input is truncated, never rejected —
 * the panel renders the first [`MAX_CHANGE_PATHS`] with an explicit count.
 */
export function changeRowsOf(input: {
  readonly filesChanged: readonly string[];
  readonly verification: NativeTaskVerification | null;
  readonly proof: NativeTaskProof | null;
}): ChangeRowSummary[] {
  const paths: string[] = [];
  const seen = new Set<string>();
  for (const raw of input.filesChanged) {
    const path = typeof raw === 'string' ? boundedPath(raw) : null;
    if (path === null || seen.has(path)) {
      continue;
    }
    seen.add(path);
    paths.push(path);
    if (paths.length >= MAX_CHANGE_PATHS) {
      break;
    }
  }
  return paths.map((path) => ({
    path,
    changeKind: null,
    refs: refsForPath(path, input.verification, input.proof),
  }));
}

function refsForPath(
  path: string,
  verification: NativeTaskVerification | null,
  proof: NativeTaskProof | null,
): ChangeRefSummary[] {
  const refs: ChangeRefSummary[] = [];
  const seen = new Set<string>();
  const push = (ref: ChangeRefSummary): void => {
    const key = `${ref.kind}\u0000${ref.label}`;
    if (refs.length >= MAX_CHANGE_REFS || seen.has(key)) {
      return;
    }
    seen.add(key);
    refs.push(ref);
  };
  const pushCriterionEvidence = (criterionKey: string, evidence: string | null): void => {
    push({
      kind: 'criterion',
      label: `criterion ${criterionKey}`,
      criterionKey,
      evidenceId: null,
    });
    for (const evidenceId of evidenceIdsIn(evidence)) {
      push({
        kind: 'evidence',
        label: `evidence #${evidenceId}`,
        criterionKey: null,
        evidenceId,
      });
    }
  };
  if (verification !== null) {
    for (const record of verification.records) {
      let listed = false;
      for (const changed of record.changedFiles) {
        if (changed.path === path) {
          listed = true;
          break;
        }
      }
      if (listed) {
        push({
          kind: 'record',
          label: `verification record ${record.recordId}`,
          criterionKey: null,
          evidenceId: null,
        });
      }
      for (const criterion of record.criteria) {
        const related =
          criterion.binding?.kind === 'file_state' &&
          criterion.binding.path === path
            ? true
            : criterion.binding?.path === path || evidenceNamesFile(criterion.evidence, path);
        if (!related) {
          continue;
        }
        const key =
          typeof criterion.criterionKey === 'string' && criterion.criterionKey.length > 0
            ? criterion.criterionKey
            : '(unnamed criterion)';
        pushCriterionEvidence(key, criterion.evidence);
      }
    }
  }
  if (proof !== null) {
    for (const criterion of proof.criteria.items) {
      const related =
        proofBindingPath(criterion.binding) === path ||
        evidenceNamesFile(criterion.evidence, path);
      if (!related) {
        continue;
      }
      const key =
        typeof criterion.criterionKey === 'string' && criterion.criterionKey.length > 0
          ? criterion.criterionKey
          : '(unnamed criterion)';
      pushCriterionEvidence(key, criterion.evidence);
    }
  }
  return refs;
}

/**
 * The newest served `candidateProof.sourceDiffEvidence` id of the run, or
 * null. Records are served newest-first; a plain decimal id and the typed
 * `evidence:<n>` ref form are both accepted (the field is documented as an
 * evidence id), any other value is an honest absence — the adapter then
 * falls through to the path's evidence refs.
 */
export function sourceDiffEvidenceIdOf(
  verification: NativeTaskVerification | null,
): number | null {
  if (verification === null) {
    return null;
  }
  for (const record of verification.records) {
    const raw = record.candidateProof?.sourceDiffEvidence ?? null;
    if (raw === null) {
      continue;
    }
    const text = raw.trim();
    if (/^[0-9]+$/.test(text)) {
      const value = Number(text);
      if (Number.isSafeInteger(value) && value >= 1) {
        return value;
      }
      continue;
    }
    const refs = evidenceIdsIn(text);
    if (refs.length > 0) {
      return refs[0]!;
    }
  }
  return null;
}

// ------------------------------------------------------- immutable basis

export interface ReviewArtifact {
  readonly kind: 'source-diff' | 'evidence';
  readonly evidenceId: number;
  readonly label: string;
  readonly title: string;
}

export interface ReviewBasis {
  /** (a) the served source-diff evidence of the run, when served. */
  readonly primary: ReviewArtifact | null;
  /** (b) the first served evidence ref attached to the selected path. */
  readonly secondary: ReviewArtifact | null;
}

/**
 * The served candidate display of the scope toolbar/doc titles: the task
 * goal when served, else a stable generic label. Raw hashes and run ids are
 * ADVANCED details and never appear here.
 */
export function candidateDisplayOf(goal: string | null): string {
  const trimmed = typeof goal === 'string' ? goal.trim() : '';
  if (trimmed.length === 0) {
    return 'Candidate change';
  }
  const clipped =
    trimmed.length > MAX_CANDIDATE_DISPLAY_CHARS
      ? `${trimmed.slice(0, MAX_CANDIDATE_DISPLAY_CHARS - 1)}…`
      : trimmed;
  return `Candidate "${clipped}"`;
}

export function sourceDiffTitle(candidateDisplay: string, evidenceId: number): string {
  return `${candidateDisplay} — source diff — evidence #${evidenceId} (immutable)`;
}

export function evidenceRefTitle(path: string, evidenceId: number): string {
  return `${path} — evidence #${evidenceId} (immutable)`;
}

/** Tab title of the two-artifact native side-by-side diff (bounded). */
export function evidenceDiffTitle(primary: ReviewArtifact, secondary: ReviewArtifact): string {
  const title = `${primary.label} ↔ ${secondary.label} (immutable)`;
  return title.length > MAX_DOCUMENT_TITLE_CHARS
    ? `${title.slice(0, MAX_DOCUMENT_TITLE_CHARS - 1)}…`
    : title;
}

/**
 * Resolve the immutable basis of one selected change in the documented
 * order. `row` is the served change row (refs only from served
 * relationships); `sourceDiffEvidenceId` is the run's served
 * `candidateProof.sourceDiffEvidence`. Both may be null: the caller renders
 * the exact `Diff unavailable` state without opening anything.
 */
export function resolveReviewBasis(input: {
  readonly path: string;
  readonly row: ChangeRowSummary | null;
  readonly sourceDiffEvidenceId: number | null;
  readonly candidateDisplay: string;
}): ReviewBasis {
  const primary =
    input.sourceDiffEvidenceId === null
      ? null
      : {
          kind: 'source-diff' as const,
          evidenceId: input.sourceDiffEvidenceId,
          label: `source diff — evidence #${input.sourceDiffEvidenceId}`,
          title: sourceDiffTitle(input.candidateDisplay, input.sourceDiffEvidenceId),
        };
  let secondary: ReviewArtifact | null = null;
  const refs = input.row?.refs ?? [];
  for (const ref of refs) {
    if (ref.kind === 'evidence' && ref.evidenceId !== null && ref.evidenceId >= 1) {
      secondary = {
        kind: 'evidence',
        evidenceId: ref.evidenceId,
        label: `evidence #${ref.evidenceId}`,
        title: evidenceRefTitle(input.path, ref.evidenceId),
      };
      break;
    }
  }
  return { primary, secondary };
}

// ---------------------------------------------------- changes snapshot

export interface ChangeAdvancedFact {
  readonly label: string;
  readonly value: string;
}

/**
 * The re-derivable "current basis" facts of the selected run (S15): proof
 * state, integration/conflict facts and the served immutable-basis handles.
 * Raw hashes and run ids live only in `advanced`, never in the primary
 * display. `Recheck current basis` re-runs the proof/projection reads and
 * rebuilds exactly this block.
 */
export interface ChangeBasisSummary {
  readonly state: 'verified' | 'unverified' | 'unavailable' | 'conflict' | 'no-basis';
  readonly lines: readonly string[];
}

export interface ChangesSummary {
  readonly available: boolean;
  readonly reason: string | null;
  readonly rows: readonly ChangeRowSummary[];
  readonly candidateDisplay: string;
  readonly basis: ChangeBasisSummary;
  readonly advanced: readonly ChangeAdvancedFact[];
}

function shortHash(value: string | null): string {
  return value === null ? '—' : value.length > 12 ? `${value.slice(0, 12)}…` : value;
}

function basisOf(
  verification: NativeTaskVerification | null,
  proof: NativeTaskProof | null,
  pathRefCount: number,
): ChangeBasisSummary {
  if (proof === null && verification === null) {
    return {
      state: 'no-basis',
      lines: ['No candidate proof or verification record is served for this run.'],
    };
  }
  const lines: string[] = [];
  let state: ChangeBasisSummary['state'] = 'no-basis';
  if (proof !== null) {
    state = proof.proofState;
    lines.push(
      proof.proofStateReason === null
        ? `candidate basis: ${proof.proofState}`
        : `candidate basis: ${proof.proofState} — ${proof.proofStateReason}`,
    );
    if (proof.trees.landedEqualsVerified === false) {
      state = 'conflict';
      lines.push('landed snapshot DIFFERS from the verified snapshot');
    } else if (proof.trees.landedEqualsVerified === true) {
      lines.push('landed snapshot matches the verified snapshot');
    }
    if (proof.integration.present) {
      if (proof.integration.conflictCount !== null && proof.integration.conflictCount > 0) {
        state = 'conflict';
        lines.push(
          `integration conflict: ${proof.integration.conflictCount} changed file${
            proof.integration.conflictCount === 1 ? '' : 's'
          } did not land cleanly`,
        );
      } else if (proof.integration.inFlight === true) {
        lines.push(
          `integration in flight${proof.integration.txn === null ? '' : ` (${proof.integration.txn.phase})`}`,
        );
      } else {
        lines.push('integration: recorded');
      }
    } else {
      lines.push('integration: not started');
    }
  } else {
    lines.push('the strict proof summary is not served for this run');
  }
  const sourceDiffId = sourceDiffEvidenceIdOf(verification);
  lines.push(
    sourceDiffId === null
      ? 'source diff: no immutable source-diff evidence is served'
      : `source diff: evidence #${sourceDiffId} served`,
  );
  lines.push(
    pathRefCount === 0
      ? 'change refs: no criterion or evidence refs are served for these paths'
      : `change refs: ${pathRefCount} served ref${pathRefCount === 1 ? '' : 's'} attached`,
  );
  return { state, lines };
}

function advancedFactsOf(
  verification: NativeTaskVerification | null,
  proof: NativeTaskProof | null,
): ChangeAdvancedFact[] {
  const facts: ChangeAdvancedFact[] = [];
  const push = (label: string, value: string | null): void => {
    facts.push({ label, value: value === null || value.length === 0 ? '—' : value });
  };
  push('evidence scheme', `${FAKTOR_EVIDENCE_SCHEME}: (read-only virtual documents)`);
  const sourceDiffId = sourceDiffEvidenceIdOf(verification);
  push('source diff evidence', sourceDiffId === null ? null : `#${sourceDiffId}`);
  if (verification !== null && verification.records.length > 0) {
    push('verification record', verification.records[0]!.recordId);
  } else {
    push('verification record', null);
  }
  push('candidate snapshot', shortHash(verification?.records[0]?.candidateProof?.candidateSnapshot ?? null));
  push('verified snapshot', shortHash(proof?.trees.verified ?? null));
  push('landed snapshot', shortHash(proof?.trees.landed ?? null));
  push('run id', proof?.integration.runId ?? null);
  push(
    'integration txn',
    proof?.integration.txn === null || proof?.integration.txn === undefined
      ? null
      : `${proof.integration.txn.txnId} (${proof.integration.txn.phase})`,
  );
  return facts;
}

/**
 * Build the full Changes summary from served data. `unavailable` names a
 * failed projection read: the view then renders its explicit unavailable
 * state (with the reason) and NO rows — the previously served rows are
 * dropped, never shown as stale.
 */
export function buildChangesSummary(input: {
  readonly filesChanged: readonly string[];
  readonly verification: NativeTaskVerification | null;
  readonly proof: NativeTaskProof | null;
  readonly unavailable: string | null;
  readonly candidateDisplay: string;
}): ChangesSummary {
  const rows = changeRowsOf({
    filesChanged: input.unavailable === null ? input.filesChanged : [],
    verification: input.verification,
    proof: input.proof,
  });
  const refCount = rows.reduce((sum, row) => sum + row.refs.length, 0);
  return {
    available: input.unavailable === null,
    reason: input.unavailable,
    rows,
    candidateDisplay: input.candidateDisplay,
    basis: basisOf(input.verification, input.proof, refCount),
    advanced: advancedFactsOf(input.verification, input.proof),
  };
}

// ----------------------------------------------- virtual document carrier

export interface EvidenceDocumentRequest {
  readonly sessionId: string;
  readonly evidenceId: number;
  readonly purpose: 'source-diff' | 'evidence';
  readonly title: string;
}

function safeDecode(segment: string): string | null {
  try {
    return decodeURIComponent(segment);
  } catch {
    return null;
  }
}

function boundedSessionId(value: string): string | null {
  const trimmed = value.trim();
  if (trimmed.length === 0 || trimmed.length > 200) {
    return null;
  }
  return /^[A-Za-z0-9._-]+$/.test(trimmed) ? trimmed : null;
}

/**
 * The canonical URI of one immutable evidence document. The final path
 * segment is the document title (the editor tab shows it); ids and purpose
 * stay machine-readable URI segments.
 */
export function evidenceDocumentUri(request: EvidenceDocumentRequest): string {
  return (
    `${FAKTOR_EVIDENCE_SCHEME}:/` +
    `${encodeURIComponent(request.sessionId)}/` +
    `${request.evidenceId}/` +
    `${request.purpose}/` +
    `${encodeURIComponent(request.title)}`
  );
}

/**
 * Strict decode of one `faktor-evidence` URI. Anything malformed (foreign
 * scheme, missing segments, a non-numeric/zero/oversized evidence id, an
 * unknown purpose, an over-bound or empty title, a hostile session id) is a
 * typed `invalid_uri` refusal — the provider never guesses a document.
 */
export function parseEvidenceDocumentUri(uri: {
  readonly scheme?: string;
  readonly path?: string;
}): EvidenceDocumentRequest {
  if (uri.scheme !== FAKTOR_EVIDENCE_SCHEME) {
    throw new ChangeReviewError('invalid_uri', `not a ${FAKTOR_EVIDENCE_SCHEME} document`);
  }
  const parts = String(uri.path ?? '')
    .split('/')
    .filter((segment) => segment.length > 0);
  if (parts.length !== 4) {
    throw new ChangeReviewError('invalid_uri', 'the evidence document URI is malformed');
  }
  const sessionId = boundedSessionId(safeDecode(parts[0]) ?? '');
  if (sessionId === null) {
    throw new ChangeReviewError('invalid_uri', 'the evidence document session id is malformed');
  }
  if (!/^[0-9]+$/.test(parts[1])) {
    throw new ChangeReviewError('invalid_uri', 'the evidence id is not a decimal integer');
  }
  const evidenceId = Number(parts[1]);
  if (!Number.isSafeInteger(evidenceId) || evidenceId < 1) {
    throw new ChangeReviewError('invalid_uri', 'the evidence id is outside the served range');
  }
  const purpose = parts[2];
  if (purpose !== 'source-diff' && purpose !== 'evidence') {
    throw new ChangeReviewError('invalid_uri', `unknown evidence document purpose ${purpose}`);
  }
  const title = safeDecode(parts[3]) ?? '';
  if (title.trim().length === 0 || title.length > MAX_DOCUMENT_TITLE_CHARS) {
    throw new ChangeReviewError('invalid_uri', 'the evidence document title is malformed');
  }
  return { sessionId, evidenceId, purpose, title };
}

/** Strict canonical base64 (the daemon's own encoding; junk is refused). */
function strictBase64(text: string): Buffer {
  if (text.length % 4 !== 0 || !/^[A-Za-z0-9+/]*={0,2}$/.test(text)) {
    throw new ChangeReviewError('malformed_payload', 'the served evidence payload is not canonical base64');
  }
  return Buffer.from(text, 'base64');
}

/**
 * The exact document text of one served retrieval: the decoded bytes, with
 * the truncation marker as the FIRST LINE when the server truncated by
 * policy. `maxBytes` is the evidence row's served bound; a retrieval past it
 * is an explicit refusal rather than an unbounded render.
 */
export function evidenceDocumentText(input: {
  readonly bytesBase64: string;
  readonly byteLen: number;
  readonly truncatedByPolicy: boolean;
  readonly maxBytes: number | null;
}): string {
  const buffer = strictBase64(input.bytesBase64);
  if (buffer.byteLength !== input.byteLen) {
    throw new ChangeReviewError(
      'malformed_payload',
      `the served evidence declared ${input.byteLen} bytes but decodes to ${buffer.byteLength}`,
    );
  }
  if (input.maxBytes !== null && buffer.byteLength > input.maxBytes) {
    throw new ChangeReviewError(
      'oversized',
      `the served evidence (${buffer.byteLength} bytes) exceeds its served maxBytes bound (${input.maxBytes})`,
    );
  }
  const text = buffer.toString('utf8');
  return input.truncatedByPolicy ? `${TRUNCATED_EVIDENCE_HEADER}\n${text}` : text;
}

/** The read half of the native client the adapter needs. */
export interface EvidenceReader {
  evidence(sessionId: string, evidenceId: number): Promise<NativeEvidence>;
  retrieveEvidence(
    sessionId: string,
    evidenceId: number,
    selector: { readonly selector: 'all' },
  ): Promise<NativeEvidenceRetrieval>;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * Load one immutable evidence document through the daemon. The id MUST
 * resolve inside the requested session (a cross-session id is refused typed
 * BEFORE any bytes are rendered); the retrieval must be byte-consistent
 * (declared length == decoded length) and inside the evidence row's served
 * `maxBytes`. No workspace file is ever touched.
 */
export async function loadEvidenceDocument(
  request: EvidenceDocumentRequest,
  client: EvidenceReader,
): Promise<string> {
  let meta: NativeEvidence;
  try {
    meta = await client.evidence(request.sessionId, request.evidenceId);
  } catch (error) {
    throw new ChangeReviewError(
      'fetch_failed',
      `evidence #${request.evidenceId} could not be read: ${messageOf(error)}`,
    );
  }
  if (meta.id !== request.evidenceId) {
    throw new ChangeReviewError(
      'unknown_evidence',
      `the authority answered evidence #${request.evidenceId} with a different id (#${meta.id})`,
    );
  }
  if (String(meta.sessionId) !== request.sessionId) {
    throw new ChangeReviewError(
      'cross_session',
      `evidence #${request.evidenceId} belongs to session ${meta.sessionId}, not session ${request.sessionId}`,
    );
  }
  let retrieval: NativeEvidenceRetrieval;
  try {
    retrieval = await client.retrieveEvidence(request.sessionId, request.evidenceId, {
      selector: 'all',
    });
  } catch (error) {
    throw new ChangeReviewError(
      'fetch_failed',
      `evidence #${request.evidenceId} bytes could not be retrieved: ${messageOf(error)}`,
    );
  }
  if (retrieval.id !== request.evidenceId) {
    throw new ChangeReviewError(
      'unknown_evidence',
      `the authority retrieved evidence #${retrieval.id} for request #${request.evidenceId}`,
    );
  }
  return evidenceDocumentText({
    bytesBase64: retrieval.bytesBase64,
    byteLen: retrieval.byteLen,
    truncatedByPolicy: retrieval.truncatedByPolicy === true,
    maxBytes: typeof meta.maxBytes === 'number' && Number.isSafeInteger(meta.maxBytes) ? meta.maxBytes : null,
  });
}
