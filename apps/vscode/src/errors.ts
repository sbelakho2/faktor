// ONE user-facing error projection for the VS Code product surface.
//
// The audit finding: raw backend text (native API codes, HTTP statuses,
// transport stack messages) reached the panel headline. This module is the
// single translation seam: every surfaced failure becomes a human summary,
// a concrete next step with an optional action, and a bounded technical
// tail that stays reachable behind a "Technical details" disclosure. Typed
// codes are DEMOTED, never deleted: they live in `technical` (prefixed in
// brackets) and in `code` for callers that still need the exact tag.
//
// Dependency-free so scripts/selftest.mjs can drive every projection
// adversarially (hostile codes, oversized messages, empty input) without a
// webview or daemon.

export type UserErrorKind =
  | 'daemon_down'
  | 'daemon_start_failed'
  | 'stream_gap'
  | 'run_failure'
  | 'permission_expired'
  | 'attachment_refused'
  | 'unexpected';

export type ErrorActionKey =
  | 'startService'
  | 'reconnectStream'
  | 'refresh'
  | 'focusComposer'
  | 'attachFiles';

/** One concrete next step rendered as a real button by the panel. */
export interface UserErrorAction {
  readonly key: ErrorActionKey;
  readonly label: string;
}

/** The exact shape the panel renders and the extension host posts. */
export interface UserErrorProjection {
  readonly kind: UserErrorKind;
  /** One human sentence naming what happened. Never raw backend text. */
  readonly summary: string;
  /** One human sentence saying what to do next. */
  readonly hint: string;
  /** A button action when one is possible; null otherwise. */
  readonly action: UserErrorAction | null;
  /** Bounded technical tail (typed code + raw message), demoted to a disclosure. */
  readonly technical: string;
  /** The typed code, preserved for callers that branch on it. */
  readonly code: string | null;
}

/** Bound of the raw technical tail; the panel never renders more. */
export const MAX_ERROR_TECHNICAL_CHARS = 500;

export function boundErrorMessage(value: string, max = MAX_ERROR_TECHNICAL_CHARS): string {
  if (value.length <= max) {
    return value;
  }
  return max > 3 ? `${value.slice(0, max - 3)}…` : value.slice(0, max);
}

interface KindCopy {
  readonly summary: string;
  readonly hint: string;
  readonly action: UserErrorAction | null;
}

/**
 * The product copy per kind. A later option implies the earlier ones, so
 * the copied shape is static data here.
 */
const KIND_COPY: Record<UserErrorKind, KindCopy> = {
  daemon_down: {
    summary: 'The Faktor service is not running',
    hint: 'Start it to review the run and keep your work.',
    action: { key: 'startService', label: 'Start Faktor' },
  },
  daemon_start_failed: {
    summary: 'The Faktor service could not start',
    hint: 'Check the technical details; the service is not running yet.',
    action: { key: 'startService', label: 'Try again' },
  },
  stream_gap: {
    summary: 'Live updates paused',
    hint: 'Reconnect to resume from the last event. Nothing is skipped.',
    action: { key: 'reconnectStream', label: 'Reconnect' },
  },
  run_failure: {
    summary: 'The run could not start',
    hint: 'Your draft and attachments were kept. Review the message and try again.',
    action: { key: 'focusComposer', label: 'Review and retry' },
  },
  permission_expired: {
    summary: 'That permission request is no longer live',
    hint: 'It was resolved elsewhere or expired. Refresh to see the current requests.',
    action: { key: 'refresh', label: 'Refresh' },
  },
  attachment_refused: {
    summary: 'Some attachments were refused',
    hint: 'Adjust the files and try again; nothing was sent.',
    action: { key: 'attachFiles', label: 'Choose files' },
  },
  unexpected: {
    summary: 'Faktor hit a problem',
    hint: 'Try the action again; open Diagnostics for the raw detail.',
    action: null,
  },
};

function kindCopyOf(kind: unknown): KindCopy {
  return KIND_COPY[kind as UserErrorKind] ?? KIND_COPY.unexpected;
}

function kindOf(kind: unknown): UserErrorKind {
  return kind === 'daemon_down' ||
    kind === 'daemon_start_failed' ||
    kind === 'stream_gap' ||
    kind === 'run_failure' ||
    kind === 'permission_expired' ||
    kind === 'attachment_refused'
    ? kind
    : 'unexpected';
}

function technicalOf(message: string | undefined, code: string | null): string {
  const raw = typeof message === 'string' && message.length > 0 ? message : 'no further detail was provided';
  const prefixed = code !== null && code.length > 0 ? `[${code}] ${raw}` : raw;
  return boundErrorMessage(prefixed);
}

/**
 * Project one failure into the single user-facing shape. Unknown kinds and
 * hostile inputs fall back to the generic copy; the typed code and raw
 * bounded message are preserved only in `technical`/`code`.
 */
export function projectError(input: {
  readonly kind?: unknown;
  readonly message?: string;
  readonly code?: string | null;
}): UserErrorProjection {
  const kind = kindOf(input.kind);
  const copy = kindCopyOf(kind);
  const code =
    typeof input.code === 'string' && input.code.length > 0 ? boundErrorMessage(input.code, 64) : null;
  return {
    kind,
    summary: copy.summary,
    hint: copy.hint,
    action: copy.action,
    technical: technicalOf(input.message, code),
    code,
  };
}

/**
 * The attachment-refusal projection: the human copy is fixed (refusals are
 * never the user's fault alone — they may be a capability gap), while the
 * exact refusal reasons are demoted to the technical disclosure. Bounded to
 * five reasons and the shared technical tail. Callers whose flow continues
 * with the accepted files pass the flow-specific human copy.
 */
export function attachmentRefusalProjection(
  reasons: readonly string[],
  copy?: { readonly summary?: string; readonly hint?: string },
): UserErrorProjection {
  const bounded = reasons
    .slice(0, 5)
    .map((reason) => (typeof reason === 'string' ? reason : String(reason)))
    .join('; ');
  return {
    kind: 'attachment_refused',
    summary: copy?.summary ?? 'Some attachments were refused',
    hint: copy?.hint ?? KIND_COPY.attachment_refused.hint,
    action: KIND_COPY.attachment_refused.action,
    technical: boundErrorMessage(bounded.length > 0 ? bounded : 'no reason was reported'),
    code: null,
  };
}
