// SSE client for the daemon's native journal event stream
// (`GET /native/session/{id}/events?after=<cursor>` — lifecycle.rs route,
// native/session.rs `native_session_events`), the stream the native server
// projects from the durable journal. Frames are `id: <seq>`,
// `event: <EventKind snake_case>` and one JSON `data:` line carrying the
// native event row (`{seq, kind, state, opId, tsMs, payload}`).
//
// Contract: every frame carries `event:` (type), `id:` (the journal
// sequence — the resume cursor) and one JSON `data:` line. Heartbeats
// (`event: heartbeat`) and comment/keep-alive lines are ignored. On any
// disconnect the client reconnects with exponential backoff and resumes
// from the last valid frame id, so a reconnect can neither duplicate nor
// skip events.
//
// Bounded frame budget: `maxFrameBytes` is the CUMULATIVE UTF-8 byte
// budget of the whole current frame — data, event, id and comment lines
// all consume it (Buffer.byteLength, never UTF-16 .length) — so a hostile
// stream of endless short `data:` lines without the blank-line terminator
// is refused as soon as the budget is crossed and no oversized payload is
// retained. An oversized DURABLE frame (one that declared an `id:`) is a
// terminal ProtocolBlocked state (see below); an oversized frame with no
// id fails typed and reconnects with backoff.
//
// Durable protocol violations are terminal: malformed JSON, a
// discriminator mismatch, a missing discriminator, an impossible cursor
// or an unknown event-row version on a frame carrying an `id:` enter the
// stable `protocol_blocked` state instead of reconnecting forever and
// replaying the same frame. The last good cursor is retained, the
// offending sequence is named, and `recover()` is the explicit
// post-upgrade reconnect. A durable event is never silently skipped.
//
// Dependency-free and fetch-injectable (scripts/selftest.mjs drives it with
// a fake fetch + ReadableStream); no vscode import.

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

export interface SseReaderLike {
  read(): Promise<{ done: boolean; value?: Uint8Array }>;
  cancel(reason?: unknown): Promise<void>;
}

export interface SseResponseLike {
  readonly status: number;
  readonly ok: boolean;
  readonly headers?: { get(name: string): string | null } | null;
  readonly body?: { getReader(): SseReaderLike } | null;
  text(): Promise<string>;
}

export interface SseInitLike {
  method: string;
  headers: Record<string, string>;
  signal?: AbortSignal;
}

export type SseFetchLike = (url: string, init: SseInitLike) => Promise<SseResponseLike>;

export interface SseFrame {
  /** `id:` — the journal sequence, monotonic per session. */
  readonly id: number;
  /** `event:` — the daemon's `EventKind` snake_case tag (`prompt_received`, ...). */
  readonly event: string;
  readonly data: Json;
}

export type EventStreamStatus =
  | 'connecting'
  | 'open'
  | 'retrying'
  | 'stopped'
  | 'protocol_blocked';

/**
 * The stable description of one durable protocol violation. `cursor` is the
 * last good resume cursor (the offending frame is NEVER skipped);
 * `offending_seq` is the frame's declared sequence when it had one, else
 * `null`. The state is terminal until an explicit `recover()`.
 */
export interface ProtocolBlocked {
  readonly cursor: number;
  readonly offending_seq: number | null;
  readonly reason: string;
}

/** One injectable delay; abort-aware so `stop()` is prompt. */
export type SleepLike = (ms: number, signal?: AbortSignal) => Promise<void>;

export interface EventStreamOptions {
  readonly baseUrl: string;
  readonly bearerToken: string;
  readonly sessionId: string;
  /** Journal sequence to resume from (0 = from the beginning). */
  readonly cursor?: number;
  readonly fetch?: SseFetchLike;
  readonly onEvent: (frame: SseFrame) => void;
  readonly onStatus?: (status: EventStreamStatus, detail?: string) => void;
  readonly onError?: (error: Error) => void;
  readonly minBackoffMs?: number;
  readonly maxBackoffMs?: number;
  /** Cumulative UTF-8 byte budget of one frame (bounded everything). */
  readonly maxFrameBytes?: number;
  /** Injectable clock for tests. */
  readonly sleep?: SleepLike;
  readonly jitter?: () => number;
}

export const DEFAULT_MIN_BACKOFF_MS = 250;
export const DEFAULT_MAX_BACKOFF_MS = 15_000;
export const DEFAULT_MAX_FRAME_BYTES = 1024 * 1024;

/**
 * HTTP statuses a reconnect can plausibly fix: 408/425/429 are transient
 * pressure, 5xx a transient server fault. Every other non-ok answer is a
 * route/credential/contract failure (a 404 on a daemon that does not serve
 * this path, a 401 with a stale bearer, ...) and must terminate in the
 * blocked state instead of retrying forever.
 */
export function isRetryableHttpStatus(status: number): boolean {
  return (
    status === 408 || status === 425 || status === 429 || (status >= 500 && status <= 599)
  );
}
/**
 * Top-level event-row version this client understands; an ABSENT version
 * means v1. A frame that explicitly declares another version is a durable
 * event this client cannot consume and enters `protocol_blocked` instead of
 * being silently dropped.
 */
export const SUPPORTED_EVENT_VERSION = 1;
/**
 * The daemon's keep-alive SSE tag; the ONLY event name this client
 * special-cases. Durable journal frames are delivered opaquely (the client
 * does not claim to reduce an EventKind it does not parse).
 */
export const HEARTBEAT_EVENT_NAME = 'heartbeat';
const ERROR_BODY_BYTES = 64 * 1024;

export class EventStreamProtocolError extends Error {
  constructor(detail: string) {
    super(`event stream protocol violation: ${detail}`);
    this.name = 'EventStreamProtocolError';
  }
}

/** A durable frame this client cannot safely consume: terminal, typed. */
export class EventStreamProtocolBlockedError extends EventStreamProtocolError {
  readonly blocked: ProtocolBlocked;

  constructor(blocked: ProtocolBlocked) {
    super(
      `event stream protocol blocked at seq ${blocked.offending_seq ?? '?'} ` +
        `(cursor ${blocked.cursor}): ${blocked.reason}`,
    );
    this.name = 'EventStreamProtocolBlockedError';
    this.blocked = blocked;
  }
}

/**
 * One abort-aware delay. The timer is cleared and the promise resolves as
 * soon as `signal` aborts, so `stop()` never waits out a backoff sleep and
 * no timer survives it.
 */
export function abortableDelay(ms: number, signal?: AbortSignal): Promise<void> {
  if (signal === undefined) {
    return new Promise((resolve) => setTimeout(resolve, ms));
  }
  if (signal.aborted) {
    return Promise.resolve();
  }
  return new Promise((resolve) => {
    let timer: ReturnType<typeof setTimeout> | null = null;
    const onAbort = (): void => {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
      signal.removeEventListener('abort', onAbort);
      resolve();
    };
    timer = setTimeout(() => {
      timer = null;
      signal.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

export class EventStream {
  private readonly options: EventStreamOptions;
  private readonly fetchImpl: SseFetchLike;
  private readonly minBackoffMs: number;
  private readonly maxBackoffMs: number;
  private readonly maxFrameBytes: number;
  private readonly sleep: SleepLike;
  private readonly jitter: () => number;
  private cursorValue: number;
  private stopped = true;
  private controller: AbortController | null = null;
  private loopPromise: Promise<void> | null = null;
  private statusValue: EventStreamStatus = 'stopped';
  private blockedValue: ProtocolBlocked | null = null;

  constructor(options: EventStreamOptions) {
    this.options = options;
    const injected = options.fetch;
    if (injected) {
      this.fetchImpl = injected;
    } else {
      const globalFetch = (globalThis as unknown as { fetch?: SseFetchLike }).fetch;
      if (typeof globalFetch !== 'function') {
        throw new EventStreamProtocolError('no fetch implementation available; pass options.fetch');
      }
      this.fetchImpl = (url, init) => globalFetch(url, init);
    }
    this.cursorValue = options.cursor ?? 0;
    this.minBackoffMs = options.minBackoffMs ?? DEFAULT_MIN_BACKOFF_MS;
    this.maxBackoffMs = options.maxBackoffMs ?? DEFAULT_MAX_BACKOFF_MS;
    this.maxFrameBytes = options.maxFrameBytes ?? DEFAULT_MAX_FRAME_BYTES;
    this.sleep = options.sleep ?? abortableDelay;
    this.jitter = options.jitter ?? (() => Math.floor(Math.random() * 100));
  }

  get cursor(): number {
    return this.cursorValue;
  }

  get status(): EventStreamStatus {
    return this.statusValue;
  }

  /** The stable blocked state, or null while the stream is not blocked. */
  get blocked(): ProtocolBlocked | null {
    return this.blockedValue;
  }

  /** Seed the resume cursor (e.g. from a paged `/native/events` read). */
  setCursor(cursor: number): void {
    if (Number.isInteger(cursor) && cursor >= 0) {
      this.cursorValue = cursor;
    }
  }

  /** Start streaming; idempotent while running. Clears a blocked state. */
  start(): void {
    if (!this.stopped) {
      return;
    }
    this.stopped = false;
    this.blockedValue = null;
    this.controller = new AbortController();
    this.loopPromise = this.loop(this.controller.signal);
  }

  /** Stop streaming and abort the in-flight connection. Idempotent. */
  stop(): void {
    if (this.stopped) {
      return;
    }
    this.stopped = true;
    this.controller?.abort();
    this.controller = null;
    this.setStatus('stopped');
  }

  /**
   * The explicit post-upgrade recovery: clears the blocked state and
   * reconnects from the last good cursor. The formerly offending frame is
   * replayed, never skipped.
   */
  recover(): void {
    if (this.blockedValue === null) {
      return;
    }
    this.start();
  }

  /** Resolves when the run loop exits (tests use this). */
  whenStopped(): Promise<void> {
    return this.loopPromise ?? Promise.resolve();
  }

  private async loop(signal: AbortSignal): Promise<void> {
    let attempt = 0;
    while (!this.stopped && !signal.aborted) {
      if (attempt > 0) {
        const backoff = Math.min(
          this.maxBackoffMs,
          this.minBackoffMs * 2 ** Math.min(attempt - 1, 16),
        );
        const wait = backoff + this.jitter();
        this.setStatus('retrying', `reconnect in ${wait}ms`);
        await this.sleep(wait, signal);
        if (this.stopped || signal.aborted) {
          break;
        }
      }
      this.setStatus(attempt === 0 ? 'connecting' : 'retrying', `from cursor ${this.cursorValue}`);
      try {
        await this.connectOnce(signal);
        // The connection served at least one response; retry at the base
        // backoff (a flapping stream must not busy-loop).
        attempt = 1;
        this.setStatus('retrying', `stream ended at cursor ${this.cursorValue}`);
      } catch (error) {
        if (this.stopped || signal.aborted) {
          break;
        }
        if (error instanceof EventStreamProtocolBlockedError) {
          this.stopped = true;
          this.controller = null;
          this.reportError(error);
          this.setStatus('protocol_blocked', error.blocked.reason);
          break;
        }
        attempt = Math.min(attempt + 1, 20);
        this.reportError(error);
      }
    }
    // A blocked run keeps its stable status; only a genuine stop is 'stopped'.
    if (this.blockedValue === null) {
      this.setStatus('stopped');
    }
  }

  /** Connects and pumps frames until the stream ends. Throws on connect failure. */
  private async connectOnce(signal: AbortSignal): Promise<void> {
    const base = this.options.baseUrl.replace(/\/+$/, '');
    const url = `${base}/native/session/${encodeURIComponent(this.options.sessionId)}/events?after=${this.cursorValue}`;
    const response = await this.fetchImpl(url, {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${this.options.bearerToken}`,
        Accept: 'text/event-stream',
        'Last-Event-ID': String(this.cursorValue),
        'Cache-Control': 'no-cache',
      },
      signal,
    });
    if (!response.ok) {
      const detail = await readErrorBody(response);
      const label = `stream rejected with HTTP ${response.status}${detail}`;
      if (!isRetryableHttpStatus(response.status)) {
        // Terminal: the route/credential/contract is wrong, so a reconnect
        // loop would 404 (or 401/...) forever. `block()` retains the cursor
        // and the explicit recover() is the post-fix reconnect.
        this.block(`${label}; reconnecting cannot fix this`, null);
      }
      throw new EventStreamProtocolError(label);
    }
    const reader = response.body?.getReader();
    if (!reader) {
      throw new EventStreamProtocolError('stream response carried no body');
    }
    this.setStatus('open', `cursor ${this.cursorValue}`);
    const decoder = new TextDecoder('utf-8', { fatal: false });
    let buffer = '';
    // Exact UTF-8 byte accounting: `bufferBytes` mirrors `buffer` (updated
    // additively per decoded chunk and per consumed line — UTF-8 encoding is
    // per code point, and neither a chunk boundary nor a '\n' can split a
    // surrogate pair), `frameBytes` is the CUMULATIVE budget of the current
    // frame; both count data, event, id and comment lines.
    let bufferBytes = 0;
    let eventName: string | null = null;
    let frameId: number | null = null;
    let frameBytes = 0;
    let dataLines: string[] = [];
    try {
      for (;;) {
        if (this.stopped || signal.aborted) {
          return;
        }
        const { done, value } = await reader.read();
        if (done) {
          return;
        }
        if (!value) {
          continue;
        }
        const decoded = decoder.decode(value, { stream: true });
        if (decoded.length === 0) {
          continue;
        }
        buffer += decoded;
        bufferBytes += Buffer.byteLength(decoded, 'utf8');
        let newline: number;
        while ((newline = buffer.indexOf('\n')) >= 0) {
          const rawLine = buffer.slice(0, newline);
          buffer = buffer.slice(newline + 1);
          const lineBytes = Buffer.byteLength(rawLine, 'utf8') + 1;
          bufferBytes -= lineBytes;
          const line = rawLine.endsWith('\r') ? rawLine.slice(0, -1) : rawLine;
          if (line === '') {
            this.dispatch(eventName, frameId, dataLines);
            eventName = null;
            frameId = null;
            frameBytes = 0;
            dataLines = [];
            continue;
          }
          frameBytes += lineBytes;
          if (line.startsWith(':')) {
            // Comments consume the frame budget like every other line.
          } else if (line.startsWith('event:')) {
            eventName = line.slice(6).trim();
          } else if (line.startsWith('id:')) {
            const rawId = line.slice(3).trim();
            const parsed = Number(rawId);
            if (rawId.length > 0 && Number.isInteger(parsed) && parsed >= 0) {
              frameId = parsed;
            } else {
              // A frame that declares an id is durable: an unreadable
              // sequence must block, never be silently skipped.
              this.block(
                `frame cursor ${JSON.stringify(rawId)} is not a valid journal sequence`,
                null,
              );
            }
          } else if (line.startsWith('data:')) {
            dataLines.push(line.slice(5).replace(/^ /, ''));
          }
          if (frameBytes > this.maxFrameBytes) {
            this.frameOversized(frameId);
          }
        }
        if (frameBytes + bufferBytes > this.maxFrameBytes) {
          this.frameOversized(frameId);
        }
      }
    } finally {
      try {
        await reader.cancel();
      } catch {
        // Best effort.
      }
    }
  }

  private frameOversized(frameId: number | null): never {
    if (frameId !== null) {
      this.block(
        `durable frame ${frameId} exceeded the ${this.maxFrameBytes} byte frame budget`,
        frameId,
      );
    }
    throw new EventStreamProtocolError(
      `unterminated frame exceeded ${this.maxFrameBytes} bytes`,
    );
  }

  private block(reason: string, offendingSeq: number | null): never {
    const blocked: ProtocolBlocked = {
      cursor: this.cursorValue,
      offending_seq: offendingSeq,
      reason,
    };
    this.blockedValue = blocked;
    throw new EventStreamProtocolBlockedError(blocked);
  }

  private dispatch(eventName: string | null, frameId: number | null, dataLines: readonly string[]): void {
    const durable = frameId !== null;
    if (dataLines.length === 0) {
      if (durable) {
        this.block(`durable frame ${frameId} carries no data lines`, frameId);
      }
      return;
    }
    const raw = dataLines.join('\n');
    let data: Json;
    try {
      data = JSON.parse(raw) as Json;
    } catch {
      if (durable) {
        this.block(`frame ${frameId} data is not JSON`, frameId);
      }
      this.reportError(new EventStreamProtocolError(`frame ${frameId ?? '?'} data is not JSON`));
      return;
    }
    if (typeof data !== 'object' || data === null || Array.isArray(data)) {
      if (durable) {
        this.block(`frame ${frameId} data is not an object`, frameId);
      }
      this.reportError(new EventStreamProtocolError(`frame ${frameId ?? '?'} data is not an object`));
      return;
    }
    const declared = (data as { event?: unknown }).event;
    // Daemon keep-alives are `event: heartbeat` + `data: {}` (no
    // discriminator, often no id): tolerate them instead of rejecting a
    // healthy stream. When both are present they must agree.
    const tagged = typeof declared === 'string' ? declared : eventName;
    if (typeof tagged !== 'string') {
      if (durable) {
        this.block(`durable frame ${frameId} carries no event discriminator`, frameId);
      }
      this.reportError(
        new EventStreamProtocolError(`frame ${frameId ?? '?'} carries no event discriminator`),
      );
      return;
    }
    if (typeof declared === 'string' && eventName !== null && eventName !== declared) {
      if (durable) {
        this.block(
          `durable frame ${frameId} event field ${eventName} disagrees with data discriminator ${declared}`,
          frameId,
        );
      }
      this.reportError(
        new EventStreamProtocolError(
          `frame ${frameId ?? '?'} event field ${eventName} disagrees with data discriminator ${declared}`,
        ),
      );
      return;
    }
    if (tagged === HEARTBEAT_EVENT_NAME) {
      // A heartbeat may carry an id (advances the resume cursor) or not
      // (pure keep-alive). It is never delivered to the UI.
      if (frameId !== null) {
        this.cursorValue = Math.max(this.cursorValue, frameId);
      }
      return;
    }
    if (frameId === null) {
      this.reportError(
        new EventStreamProtocolError(`frame (${tagged}) carries no id cursor; skipping`),
      );
      return;
    }
    const version = (data as { version?: unknown }).version;
    if (version !== undefined && version !== SUPPORTED_EVENT_VERSION) {
      this.block(
        `durable frame ${frameId} declares unsupported event version ${JSON.stringify(version)}`,
        frameId,
      );
    }
    if (frameId <= this.cursorValue) {
      // Replayed frame behind the cursor; never redeliver.
      return;
    }
    this.cursorValue = frameId;
    this.options.onEvent({ id: frameId, event: tagged, data });
  }

  private setStatus(status: EventStreamStatus, detail?: string): void {
    this.statusValue = status;
    this.options.onStatus?.(status, detail);
  }

  private reportError(error: unknown): void {
    const wrapped =
      error instanceof Error ? error : new EventStreamProtocolError(String(error));
    this.options.onError?.(wrapped);
  }
}

async function readErrorBody(response: SseResponseLike): Promise<string> {
  try {
    const reader = response.body?.getReader();
    if (reader) {
      const chunks: Uint8Array[] = [];
      let size = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done || !value) {
          break;
        }
        const remaining = ERROR_BODY_BYTES - size;
        if (remaining <= 0) {
          break;
        }
        chunks.push(value.byteLength > remaining ? value.subarray(0, remaining) : value);
        size += Math.min(value.byteLength, remaining);
        if (size >= ERROR_BODY_BYTES) {
          break;
        }
      }
      try {
        await reader.cancel();
      } catch {
        // Best effort.
      }
      return snippetOf(new TextDecoder('utf-8', { fatal: false }).decode(concat(chunks)));
    }
    return snippetOf(await response.text());
  } catch {
    return '';
  }
}

function concat(chunks: readonly Uint8Array[]): Uint8Array {
  let size = 0;
  for (const chunk of chunks) {
    size += chunk.byteLength;
  }
  const joined = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    joined.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return joined;
}

function snippetOf(text: string): string {
  const snippet = text.slice(0, 200).trim();
  return snippet.length > 0 ? `: ${snippet}` : '';
}
