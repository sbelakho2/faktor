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
      // The contract says keep-alives carry NO id. A hostile/buggy heartbeat
      // id must never move the resume cursor: doing so silently skips every
      // durable event below it and `recover()` can never replay them. The
      // JetBrains twin enforces the same rule (NativeEventStreamTest).
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

// ------------------------------------------- live model output subscription
// (`GET /native/session/{id}/live/output` — native/live_output.rs; the
// snapshot twin is `GET /native/session/{id}/live/snapshot`).
//
// Contract (the subscription, field by field — every frame also carries
// `instance` + `session`):
//   * `snapshot` (FIRST frame and every reconnect handshake): `instance`
//     (server-instance id; changes on daemon restart), `session`,
//     `sequence` (hub sequence watermark — the exact starting point;
//     subscribe-before-submit means a fast reply cannot outrun the
//     listener), `watermark` (durable journal watermark), `turn`
//     ({op,status} | null — the bounded active-turn projection) and
//     `parts` (authoritative part snapshots: partId/messageId/kind/turn/
//     attempt/revision/offset/text/durableBytes/durableRevision/liveTail);
//   * `live` (one chunk delta): `messageId`, `kind`, `partId`, `turn`,
//     `attempt`, `sequence` (hub-global), `revision` (per-part), `offset`
//     (UTF-8 BYTE offset — never a UTF-16 index; content deltas are whole
//     code points so JS appends + a running byteLength are exact), and
//     `content`;
//   * `resync` (explicit lag signal): the bounded per-subscriber queue
//     overran — the client fetches `/live/snapshot` and reconciles;
//   * `turn` (durable turn projection changed), `heartbeat` (ignored).
//
// Live frames are EPHEMERAL (unlike durable journal frames): a malformed
// or oversized one is dropped and reported, never terminal — the offset
// gap it leaves triggers the client's resync path against the snapshot.

export interface LiveSnapshotPart {
  readonly partId: string;
  readonly messageId: number;
  readonly kind: string;
  readonly turn: string | null;
  readonly attempt: number;
  readonly revision: number;
  /** UTF-8 byte length of `text` == the next expected frame offset. */
  readonly offset: number;
  readonly text: string;
  readonly durableBytes: number;
  readonly durableRevision: number;
  readonly liveTail: boolean;
}

export interface LiveSnapshot {
  readonly instance: string;
  readonly session: number;
  readonly sequence: number;
  readonly watermark: number;
  readonly turn: { readonly op: string; readonly status: string } | null;
  readonly parts: readonly LiveSnapshotPart[];
}

export interface LiveChunkFrame {
  readonly instance: string;
  readonly session: number;
  readonly messageId: number;
  readonly kind: string;
  readonly partId: string;
  readonly turn: string | null;
  readonly attempt: number;
  readonly sequence: number;
  readonly revision: number;
  /** UTF-8 byte offset where `content` begins in the logical part. */
  readonly offset: number;
  readonly content: string;
}

export interface LiveResyncFrame {
  readonly instance: string;
  readonly session: number;
  readonly reason: string;
  readonly dropped: number;
}

export interface LiveTurnFrame {
  readonly instance: string;
  readonly session: number;
  readonly turn: string | null;
  readonly status: string | null;
}

export type LiveOutputMessage =
  | { readonly type: 'snapshot'; readonly snapshot: LiveSnapshot }
  | { readonly type: 'live'; readonly frame: LiveChunkFrame }
  | { readonly type: 'resync'; readonly frame: LiveResyncFrame }
  | { readonly type: 'turn'; readonly frame: LiveTurnFrame };

export type LiveOutputStreamStatus =
  | 'connecting'
  | 'open'
  | 'retrying'
  | 'stopped'
  | 'unavailable';

export interface LiveOutputStreamOptions {
  readonly baseUrl: string;
  readonly bearerToken: string;
  readonly sessionId: string;
  /** Informational resume hint (the snapshot is the real reconciliation). */
  readonly cursor?: number;
  readonly fetch?: SseFetchLike;
  readonly onMessage: (message: LiveOutputMessage) => void;
  readonly onStatus?: (status: LiveOutputStreamStatus, detail?: string) => void;
  readonly onError?: (error: Error) => void;
  readonly minBackoffMs?: number;
  readonly maxBackoffMs?: number;
  /** Cumulative UTF-8 byte budget of one frame (bounded everything). */
  readonly maxFrameBytes?: number;
  readonly sleep?: SleepLike;
  readonly jitter?: () => number;
}

export class LiveOutputError extends Error {
  constructor(detail: string) {
    super(`live output stream: ${detail}`);
    this.name = 'LiveOutputError';
  }
}

const LIVE_KNOWN_EVENTS: readonly string[] = [
  'snapshot',
  'live',
  'resync',
  'turn',
  'heartbeat',
];

function liveFrameString(data: Record<string, Json>, key: string, what: string): string {
  const value = data[key];
  if (typeof value !== 'string' || value.length === 0 || value.length > 256) {
    throw new LiveOutputError(`${what}: missing/invalid ${key}`);
  }
  return value;
}

function liveFrameNumber(data: Record<string, Json>, key: string, what: string): number {
  const value = data[key];
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) {
    throw new LiveOutputError(`${what}: missing/invalid ${key}`);
  }
  return value;
}

function liveInstance(data: Record<string, Json>, what: string): string {
  const instance = liveFrameString(data, 'instance', what);
  if (!/^[0-9a-f]{16}$/.test(instance)) {
    throw new LiveOutputError(`${what}: malformed instance id`);
  }
  return instance;
}

/** Validate one `snapshot` frame payload (typed; throws on a hostile shape). */
export function parseLiveSnapshot(data: Record<string, Json>): LiveSnapshot {
  const instance = liveInstance(data, 'snapshot');
  const session = liveFrameNumber(data, 'session', 'snapshot');
  const sequence = liveFrameNumber(data, 'sequence', 'snapshot');
  const watermark = liveFrameNumber(data, 'watermark', 'snapshot');
  const rawTurn = data.turn;
  let turn: LiveSnapshot['turn'] = null;
  if (rawTurn !== null && typeof rawTurn === 'object' && !Array.isArray(rawTurn)) {
    const turnRecord = rawTurn as Record<string, Json>;
    turn = {
      op: liveFrameString(turnRecord, 'op', 'snapshot.turn'),
      status: liveFrameString(turnRecord, 'status', 'snapshot.turn'),
    };
  }
  const rawParts = data.parts;
  if (!Array.isArray(rawParts)) {
    throw new LiveOutputError('snapshot: parts is not an array');
  }
  if (rawParts.length > 64) {
    throw new LiveOutputError('snapshot: parts array exceeds the bound');
  }
  const parts: LiveSnapshotPart[] = rawParts.map((rawPart, index) => {
    if (rawPart === null || typeof rawPart !== 'object' || Array.isArray(rawPart)) {
      throw new LiveOutputError(`snapshot.parts[${index}] is not an object`);
    }
    const part = rawPart as Record<string, Json>;
    const text = part.text;
    if (typeof text !== 'string') {
      throw new LiveOutputError(`snapshot.parts[${index}].text is not a string`);
    }
    const rawLiveTail = part.liveTail;
    return {
      partId: liveFrameString(part, 'partId', `snapshot.parts[${index}]`),
      messageId: liveFrameNumber(part, 'messageId', `snapshot.parts[${index}]`),
      kind: liveFrameString(part, 'kind', `snapshot.parts[${index}]`),
      turn:
        typeof part.turn === 'string' ? part.turn : null,
      attempt: liveFrameNumber(part, 'attempt', `snapshot.parts[${index}]`),
      revision: liveFrameNumber(part, 'revision', `snapshot.parts[${index}]`),
      offset: liveFrameNumber(part, 'offset', `snapshot.parts[${index}]`),
      text,
      durableBytes: liveFrameNumber(part, 'durableBytes', `snapshot.parts[${index}]`),
      durableRevision: liveFrameNumber(part, 'durableRevision', `snapshot.parts[${index}]`),
      liveTail: rawLiveTail === true,
    };
  });
  return { instance, session, sequence, watermark, turn, parts };
}

/** Validate one `live` chunk frame (typed; throws on a hostile shape). */
export function parseLiveChunk(data: Record<string, Json>): LiveChunkFrame {
  const what = 'live';
  const turnRaw = data.turn;
  return {
    instance: liveInstance(data, what),
    session: liveFrameNumber(data, 'session', what),
    messageId: liveFrameNumber(data, 'messageId', what),
    kind: liveFrameString(data, 'kind', what),
    partId: liveFrameString(data, 'partId', what),
    turn: typeof turnRaw === 'string' ? turnRaw : null,
    attempt: liveFrameNumber(data, 'attempt', what),
    sequence: liveFrameNumber(data, 'sequence', what),
    revision: liveFrameNumber(data, 'revision', what),
    offset: liveFrameNumber(data, 'offset', what),
    content: typeof data.content === 'string' ? data.content : '',
  };
}

/**
 * The authoritative part snapshot a client fetches after an explicit
 * `resync` (or to prime a fresh view). Strict response validation; a
 * hostile shape throws a typed [`LiveOutputError`].
 */
export async function fetchLiveSnapshot(
  baseUrl: string,
  bearerToken: string,
  sessionId: string,
  options: { readonly fetch?: SseFetchLike; readonly message?: number; readonly kind?: string } = {},
): Promise<LiveSnapshot> {
  const fetchImpl =
    options.fetch ??
    ((url: string, init: SseInitLike) =>
      (
        globalThis as unknown as { fetch: (u: string, i: SseInitLike) => Promise<SseResponseLike> }
      ).fetch(url, init));
  const base = baseUrl.replace(/\/+$/, '');
  const query =
    options.message !== undefined && options.kind !== undefined
      ? `?message=${options.message}&kind=${encodeURIComponent(options.kind)}`
      : '';
  const url = `${base}/native/session/${encodeURIComponent(sessionId)}/live/snapshot${query}`;
  const response = await fetchImpl(url, {
    method: 'GET',
    headers: {
      Authorization: `Bearer ${bearerToken}`,
      Accept: 'application/json',
    },
  });
  if (!response.ok) {
    const detail = await readErrorBody(response);
    throw new LiveOutputError(`snapshot rejected with HTTP ${response.status}${detail}`);
  }
  const reader = response.body?.getReader();
  let raw: string;
  if (reader) {
    const chunks: Uint8Array[] = [];
    for (;;) {
      const { done, value } = await reader.read();
      if (done || !value) {
        break;
      }
      chunks.push(value);
      const size = chunks.reduce((sum, chunk) => sum + chunk.byteLength, 0);
      if (size > 4 * 1024 * 1024) {
        throw new LiveOutputError('snapshot body exceeds the 4 MiB bound');
      }
    }
    raw = new TextDecoder('utf-8', { fatal: false }).decode(concat(chunks));
  } else {
    raw = await response.text();
  }
  let parsed: Json;
  try {
    parsed = JSON.parse(raw) as Json;
  } catch {
    throw new LiveOutputError('snapshot body is not JSON');
  }
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new LiveOutputError('snapshot body is not an object');
  }
  return parseLiveSnapshot(parsed as Record<string, Json>);
}

/**
 * The live-output subscription: one bounded SSE reader per session whose
 * FIRST delivered message is always the handshake `snapshot` (exact
 * starting watermark — subscribe-before-submit). Disconnects reconnect with
 * the same bounded backoff as [`EventStream`]; every reconnect re-handsakes
 * with a fresh snapshot, so a missed range can never silently skip text.
 */
export class LiveOutputStream {
  private readonly options: LiveOutputStreamOptions;
  private readonly fetchImpl: SseFetchLike;
  private readonly minBackoffMs: number;
  private readonly maxBackoffMs: number;
  private readonly maxFrameBytes: number;
  private readonly sleep: SleepLike;
  private readonly jitter: () => number;
  private cursorValue: number;
  private instanceValue: string | null = null;
  private stopped = true;
  private controller: AbortController | null = null;
  private loopPromise: Promise<void> | null = null;
  private statusValue: LiveOutputStreamStatus = 'stopped';

  constructor(options: LiveOutputStreamOptions) {
    this.options = options;
    const injected = options.fetch;
    if (injected) {
      this.fetchImpl = injected;
    } else {
      const globalFetch = (globalThis as unknown as { fetch?: SseFetchLike }).fetch;
      if (typeof globalFetch !== 'function') {
        throw new LiveOutputError('no fetch implementation available; pass options.fetch');
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

  /** The server-instance id of the LAST handshake (null before one). */
  get instance(): string | null {
    return this.instanceValue;
  }

  get status(): LiveOutputStreamStatus {
    return this.statusValue;
  }

  start(): void {
    if (!this.stopped) {
      return;
    }
    this.stopped = false;
    this.controller = new AbortController();
    this.loopPromise = this.loop(this.controller.signal);
  }

  stop(): void {
    if (this.stopped) {
      return;
    }
    this.stopped = true;
    this.controller?.abort();
    this.controller = null;
    this.setStatus('stopped');
  }

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
      this.setStatus(attempt === 0 ? 'connecting' : 'retrying');
      try {
        await this.connectOnce(signal);
        attempt = 1;
        this.setStatus('retrying', 'stream ended');
      } catch (error) {
        if (this.stopped || signal.aborted) {
          break;
        }
        if (error instanceof LiveOutputUnavailableError) {
          // A non-retryable route/credential failure (404 on a daemon that
          // does not serve live output, 401, ...): stop instead of spinning
          // forever. The journal stream and the 120ms page refresh keep the
          // product working without live frames.
          this.stopped = true;
          this.controller = null;
          this.reportError(error);
          this.setStatus('unavailable', error.message);
          break;
        }
        attempt = Math.min(attempt + 1, 20);
        this.reportError(error);
      }
    }
    if (this.statusValue !== 'unavailable') {
      this.setStatus('stopped');
    }
  }

  private async connectOnce(signal: AbortSignal): Promise<void> {
    const base = this.options.baseUrl.replace(/\/+$/, '');
    const url =
      `${base}/native/session/${encodeURIComponent(this.options.sessionId)}` +
      `/live/output?after=${this.cursorValue}`;
    const response = await this.fetchImpl(url, {
      method: 'GET',
      headers: {
        Authorization: `Bearer ${this.options.bearerToken}`,
        Accept: 'text/event-stream',
        'Cache-Control': 'no-cache',
      },
      signal,
    });
    if (!response.ok) {
      const detail = await readErrorBody(response);
      const label = `subscription rejected with HTTP ${response.status}${detail}`;
      if (!isRetryableHttpStatus(response.status)) {
        throw new LiveOutputUnavailableError(`${label}; not retrying`);
      }
      throw new LiveOutputError(label);
    }
    const reader = response.body?.getReader();
    if (!reader) {
      throw new LiveOutputError('subscription response carried no body');
    }
    this.setStatus('open');
    const decoder = new TextDecoder('utf-8', { fatal: false });
    let buffer = '';
    let bufferBytes = 0;
    let eventName: string | null = null;
    let dataLines: string[] = [];
    let frameBytes = 0;
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
            this.dispatchLive(eventName, dataLines);
            eventName = null;
            dataLines = [];
            frameBytes = 0;
            continue;
          }
          frameBytes += lineBytes;
          if (line.startsWith(':')) {
            // Keep-alive comment: consumes budget, carries nothing.
          } else if (line.startsWith('event:')) {
            eventName = line.slice(6).trim();
          } else if (line.startsWith('data:')) {
            dataLines.push(line.slice(5).replace(/^ /, ''));
          }
          if (frameBytes > this.maxFrameBytes) {
            throw new LiveOutputError(
              `unterminated frame exceeded ${this.maxFrameBytes} bytes`,
            );
          }
        }
        if (frameBytes + bufferBytes > this.maxFrameBytes) {
          throw new LiveOutputError(`frame exceeded ${this.maxFrameBytes} bytes`);
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

  /**
   * One parsed frame. Live frames are ephemeral: a malformed one is
   * dropped and reported (the resulting offset gap triggers the client's
   * resync path) — it can never block the subscription the way a durable
   * journal violation can.
   */
  private dispatchLive(eventName: string | null, dataLines: readonly string[]): void {
    if (dataLines.length === 0) {
      if (eventName !== null && eventName !== 'heartbeat') {
        this.reportError(new LiveOutputError(`frame ${eventName} carries no data`));
      }
      return;
    }
    if (eventName === 'heartbeat') {
      return;
    }
    if (eventName === null || LIVE_KNOWN_EVENTS.indexOf(eventName) < 0) {
      this.reportError(
        new LiveOutputError(`unknown live frame ${JSON.stringify(eventName)}; dropping`),
      );
      return;
    }
    const raw = dataLines.join('\n');
    let data: Json;
    try {
      data = JSON.parse(raw) as Json;
    } catch {
      this.reportError(new LiveOutputError(`frame ${eventName} data is not JSON; dropping`));
      return;
    }
    if (data === null || typeof data !== 'object' || Array.isArray(data)) {
      this.reportError(new LiveOutputError(`frame ${eventName} data is not an object; dropping`));
      return;
    }
    const record = data as Record<string, Json>;
    try {
      if (eventName === 'snapshot') {
        const snapshot = parseLiveSnapshot(record);
        this.instanceValue = snapshot.instance;
        this.cursorValue = snapshot.sequence;
        this.options.onMessage({ type: 'snapshot', snapshot });
        return;
      }
      if (eventName === 'live') {
        const frame = parseLiveChunk(record);
        // Frames from a PREVIOUS daemon instance are a foreign lineage:
        // drop them (the handshake snapshot of this instance is authority).
        if (this.instanceValue !== null && frame.instance !== this.instanceValue) {
          this.reportError(
            new LiveOutputError('live frame from a foreign server instance; dropping'),
          );
          return;
        }
        if (frame.sequence <= this.cursorValue) {
          // Already-seen sequence (replay): never redeliver.
          return;
        }
        this.cursorValue = frame.sequence;
        this.options.onMessage({ type: 'live', frame });
        return;
      }
      if (eventName === 'resync') {
        const frame = {
          instance: liveInstance(record, 'resync'),
          session: liveFrameNumber(record, 'session', 'resync'),
          reason: liveFrameString(record, 'reason', 'resync'),
          dropped: liveFrameNumber(record, 'dropped', 'resync'),
        };
        this.options.onMessage({ type: 'resync', frame });
        return;
      }
      // turn
      const turnRaw = record.turn;
      const statusRaw = record.status;
      this.options.onMessage({
        type: 'turn',
        frame: {
          instance: liveInstance(record, 'turn'),
          session: liveFrameNumber(record, 'session', 'turn'),
          turn: typeof turnRaw === 'string' ? turnRaw : null,
          status: typeof statusRaw === 'string' ? statusRaw : null,
        },
      });
    } catch (error) {
      this.reportError(error);
    }
  }

  private setStatus(status: LiveOutputStreamStatus, detail?: string): void {
    this.statusValue = status;
    this.options.onStatus?.(status, detail);
  }

  private reportError(error: unknown): void {
    const wrapped = error instanceof Error ? error : new LiveOutputError(String(error));
    this.options.onError?.(wrapped);
  }
}

/** A non-retryable live subscription failure (route/credential/contract). */
export class LiveOutputUnavailableError extends LiveOutputError {}
