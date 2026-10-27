// Ask transport/session recovery.
//
// An Ask turn is posted by the extension HOST through the native client; the
// webview never fetches the daemon itself. Two failures are recoverable
// without losing the draft:
//
//   - transport: the supervised daemon is unreachable, restarted on a new
//     loopback port, or its connection was reset. The supervisor is asked to
//     re-establish the connection (which refreshes `baseUrl`) and the SAME
//     logical submission is retried exactly once.
//   - stale_session: the cached session id no longer exists (a fresh data
//     dir, a deleted conversation, a stale workspace binding). The host
//     rebinds/creates a session for this workspace and retries exactly once.
//
// Every other failure (a typed API refusal, a malformed request, a protocol
// violation) is NOT retried: it is surfaced with its own cause. The retry
// budget is global, so a transport failure followed by a missing session can
// never cause more than one extra prompt.

import {
  NativeApiError,
  NativeProtocolError,
  NativeTransportError,
  type NativePromptReceipt,
} from './nativeClient.ts';
import { sendAskTurn, type AskPromptClient } from './askFlow.ts';

export type AskFailureCause = 'transport' | 'stale_session';

const STALE_SESSION_CODES = new Set([
  'not_found',
  'session_not_found',
  'unknown_session',
  'invalid_session',
]);

/** True when an error came from the transport, not from the protocol. */
export function isTransportFailure(error: unknown): boolean {
  if (error instanceof NativeTransportError) {
    return true;
  }
  // Compatibility with a client built before the typed transport error: the
  // old wrapper reported every fetch rejection as a protocol violation whose
  // detail began with `fetch failed`.
  return (
    error instanceof NativeProtocolError &&
    /(?:^|:\s)(fetch failed|request timed out)/.test(error.detail)
  );
}

/** True only for a 404 that names a missing/stale session. */
export function isStaleSessionFailure(error: unknown): boolean {
  if (!(error instanceof NativeApiError) || error.status !== 404) {
    return false;
  }
  return STALE_SESSION_CODES.has(error.code) && /session/i.test(error.message);
}

/** The recovery a failure permits, or null when it must surface as-is. */
export function classifyAskFailure(error: unknown): AskFailureCause | null {
  if (isStaleSessionFailure(error)) {
    return 'stale_session';
  }
  if (isTransportFailure(error)) {
    return 'transport';
  }
  return null;
}

/** The live connection an Ask turn runs on. */
export interface AskRecoveryConnection {
  readonly client: AskPromptClient;
  readonly sessionId: string;
}

/**
 * Host ports. `ensureConnection` MUST await service auto-start (and resume);
 * `recoverTransport` MUST replace the stale client/base URL (restart or
 * reconnect the supervisor) before it resolves; `rebindSession` MUST list the
 * durable sessions and create/rebind a workspace session.
 */
export interface AskRecoveryPorts {
  ensureConnection(): Promise<AskRecoveryConnection>;
  recoverTransport(connection: AskRecoveryConnection): Promise<AskRecoveryConnection>;
  rebindSession(connection: AskRecoveryConnection): Promise<AskRecoveryConnection>;
}

export interface AskTurnRequest {
  readonly prompt: string;
  readonly files: readonly string[];
  readonly submissionId: string;
}

export interface AskTurnOutcome {
  readonly receipt: NativePromptReceipt;
  readonly connection: AskRecoveryConnection;
  readonly recovered: AskFailureCause | null;
  /** 1 = accepted on the first attempt; 2 = accepted after one recovery. */
  readonly attempts: number;
}

/**
 * Post ONE ordinary prompt, recovering at most once from a transport failure
 * or a stale session. The submission id is stable across the retry (the
 * daemon's prompt admission makes the retry idempotent), and any other
 * failure propagates unchanged.
 */
export async function sendAskTurnWithRecovery(
  ports: AskRecoveryPorts,
  request: AskTurnRequest,
): Promise<AskTurnOutcome> {
  let connection = await ports.ensureConnection();
  let recovered: AskFailureCause | null = null;
  for (let attempts = 1; attempts <= 2; attempts += 1) {
    try {
      const receipt = await sendAskTurn(connection.client, {
        sessionId: connection.sessionId,
        prompt: request.prompt,
        files: request.files,
        submissionId: request.submissionId,
      });
      return { receipt, connection, recovered, attempts };
    } catch (error) {
      if (attempts === 2) {
        throw error;
      }
      const cause = classifyAskFailure(error);
      if (cause === null) {
        throw error;
      }
      recovered = cause;
      connection =
        cause === 'transport'
          ? await ports.recoverTransport(connection)
          : await ports.rebindSession(connection);
    }
  }
  // The loop either returns or throws on the second attempt.
  throw new NativeTransportError('POST /native/session/{id}/prompt', 'ask recovery exhausted');
}
