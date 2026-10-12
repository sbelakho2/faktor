// Bounded post-health readiness wait (I06d).
//
// Health is the LIVENESS gate; readiness is a separate, explicit contract:
// `GET /native/ready` must return `{ready: true}` within a bounded deadline
// before the extension may declare the service `ready`. A timeout is a typed,
// visible outcome (`timeout`) — never an implicit success — and a route the
// serving daemon does not expose (404/405) is a terminal typed `failed`
// outcome instead of a poll loop that can never succeed.
//
// Dependency-free (no vscode import) so scripts/selftest.mjs can drive it
// with a fake client; abort-aware so `Cancel startup` unwinds promptly and no
// timer survives it.

import { abortableDelay } from './eventStream.ts';

export type ReadinessState = 'ready' | 'timeout' | 'failed';

export interface ReadinessReport {
  readonly state: ReadinessState;
  /** Human/technical detail; the extension demotes it to Diagnostics. */
  readonly detail: string;
}

/** The one method readiness needs; NativeClient satisfies it. */
export interface ReadyClient {
  ready(): Promise<{ readonly ready: boolean }>;
}

/** Thrown when the configured AbortSignal fires while waiting. */
export class ReadinessCancelledError extends Error {
  readonly code = 'start_cancelled';

  constructor(detail = 'the daemon startup was cancelled while waiting for readiness') {
    super(detail);
    this.name = 'ReadinessCancelledError';
  }
}

export interface AwaitReadyOptions {
  /** The bounded deadline (the same startup bound the spawn used). */
  readonly timeoutMs: number;
  readonly signal?: AbortSignal;
  readonly pollMs?: number;
  /** Injectable for tests. */
  readonly sleep?: (ms: number, signal?: AbortSignal) => Promise<void>;
  readonly now?: () => number;
}

function statusOf(error: unknown): number | null {
  if (typeof error === 'object' && error !== null) {
    const status = (error as { status?: unknown }).status;
    if (typeof status === 'number' && Number.isInteger(status)) {
      return status;
    }
  }
  return null;
}

/**
 * Poll `GET /native/ready` until `ready: true` or the deadline. `ready:false`
 * and transient errors keep polling; a non-retryable route refusal (404/405)
 * fails terminally; the deadline produces the typed `timeout` outcome.
 */
export async function awaitReady(
  client: ReadyClient,
  options: AwaitReadyOptions,
): Promise<ReadinessReport> {
  const timeoutMs = options.timeoutMs;
  const pollMs = options.pollMs ?? 100;
  const sleep = options.sleep ?? abortableDelay;
  const now = options.now ?? (() => Date.now());
  const signal = options.signal;
  const deadline = now() + timeoutMs;
  let last = 'no readiness report yet';
  for (;;) {
    if (signal?.aborted === true) {
      throw new ReadinessCancelledError();
    }
    try {
      const report = await client.ready();
      if (report.ready) {
        return { state: 'ready', detail: 'GET /native/ready reports ready' };
      }
      last = 'GET /native/ready reports ready:false';
    } catch (error) {
      const status = statusOf(error);
      if (status === 404 || status === 405) {
        return {
          state: 'failed',
          detail:
            `GET /native/ready is not served by this daemon (HTTP ${status}); ` +
            'readiness cannot be confirmed',
        };
      }
      last = error instanceof Error ? error.message : String(error);
    }
    if (now() >= deadline) {
      return {
        state: 'timeout',
        detail:
          `GET /native/ready did not report ready within ${timeoutMs}ms; ` +
          `the service keeps running but is not ready (last: ${last})`,
      };
    }
    await sleep(Math.min(pollMs, Math.max(1, deadline - now())), signal);
  }
}
