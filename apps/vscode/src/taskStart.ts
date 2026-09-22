// Pure task-start policy for the VS Code product surface.
//
// P0 shadow-only mutation policy: the daemon's sole mode is shadow mutation
// (every mutating run works in an isolated candidate), so the client's
// `faktor.mutationMode` setting vocabulary is "" (inherit-daemon; the
// production default) or "shadow" (explicit). The removed `direct_compat`
// value is a typed strict-parse refusal at start admission — never
// forwarded, never fabricated, and never a downgrade target. A daemon 409
// (conflict) stays a typed, actionable refusal attempted exactly ONCE.
//
// Dependency-free (no vscode import) so scripts/selftest.mjs drives it
// with a fake client.

import { NativeApiError } from './nativeClient.ts';
import type {
  NativeAttachmentId,
  NativeCompletionContract,
  NativeTaskRunStarted,
  StartTaskRunRequest,
} from './nativeClient.ts';
import { microWireValue, type MicroMoney } from './money.ts';

/** One durable typed attachment id a task start accepts (native DTO mirror). */
export type TaskAttachmentId = NativeAttachmentId;

/** One binary attachment of a pending submission: exact bytes as base64. */
export interface PendingBinaryAttachment {
  readonly mime: string;
  readonly filename: string | null;
  readonly bytes: number;
  readonly dataBase64: string;
  readonly isImage: boolean;
}

/**
 * The host-side PENDING SUBMISSION ENVELOPE. It is built from the composer
 * submission (text + optional session/draft/message identity + the original
 * `files` payload) and retained until the daemon durably accepts the task.
 * On ANY admission failure the host restores the draft from these exact
 * fields, so the user's text and attachments are never silently lost. The
 * envelope is never cleared before acceptance and never emptied on failure.
 */
export interface PendingSubmission {
  readonly text: string;
  readonly sessionId: string | null;
  readonly draftId: string | null;
  readonly messageId: string | null;
  readonly files: readonly unknown[];
  readonly attachments: readonly PendingBinaryAttachment[];
}

/** Decoded-byte ceiling of one upload (mirrors the daemon's 7 MiB bound). */
export const MAX_PENDING_ATTACHMENT_BYTES = 7 * 1024 * 1024;

const MAX_PENDING_FILES = 64;
const MAX_PENDING_ID_CHARS = 4096;
const MAX_PENDING_BASE64_CHARS = Math.ceil(MAX_PENDING_ATTACHMENT_BYTES / 3) * 4 + 8;

function pendingString(value: unknown, max = MAX_PENDING_ID_CHARS): string | null {
  if (typeof value !== 'string') {
    return null;
  }
  const trimmed = value.trim();
  if (trimmed.length === 0 || trimmed.length > max) {
    return null;
  }
  return trimmed;
}

/**
 * Defensive re-validation of one pending envelope arriving from the
 * webview layer. Returns `null` for any hostile/oversized shape so the
 * caller drops the start loudly instead of fabricating an identity; a
 * `null` id means the field was absent (optional by design), not invalid.
 */
export function parsePendingSubmission(raw: unknown): PendingSubmission | null {
  if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
    return null;
  }
  const record = raw as Record<string, unknown>;
  const text = typeof record.text === 'string' ? record.text : null;
  if (text === null || text.trim().length === 0 || text.length > 64 * 1024) {
    return null;
  }
  const optionalId = (value: unknown): string | null | undefined => {
    if (value === undefined || value === null) {
      return null;
    }
    return pendingString(value) ?? undefined;
  };
  const sessionId = optionalId(record.sessionId);
  const draftId = optionalId(record.draftId);
  const messageId = optionalId(record.messageId);
  if (sessionId === undefined || draftId === undefined || messageId === undefined) {
    return null;
  }
  const files = record.files;
  if (files !== undefined && files !== null && !Array.isArray(files)) {
    return null;
  }
  if (Array.isArray(files) && files.length > MAX_PENDING_FILES) {
    return null;
  }
  const attachments: PendingBinaryAttachment[] = [];
  const rawAttachments = record.attachments;
  if (rawAttachments !== undefined && rawAttachments !== null) {
    if (!Array.isArray(rawAttachments) || rawAttachments.length > MAX_PENDING_FILES) {
      return null;
    }
    for (const entry of rawAttachments) {
      if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) {
        return null;
      }
      const item = entry as Record<string, unknown>;
      const mime = pendingString(item.mime, 128);
      const filename =
        item.filename === undefined || item.filename === null
          ? null
          : pendingString(item.filename, 255);
      const dataBase64 = typeof item.dataBase64 === 'string' ? item.dataBase64 : null;
      const bytes =
        typeof item.bytes === 'number' && Number.isInteger(item.bytes) && item.bytes >= 0
          ? item.bytes
          : null;
      if (
        mime === null ||
        (filename === null && item.filename !== undefined && item.filename !== null) ||
        dataBase64 === null ||
        dataBase64.length > MAX_PENDING_BASE64_CHARS ||
        bytes === null ||
        bytes > MAX_PENDING_ATTACHMENT_BYTES
      ) {
        return null;
      }
      attachments.push({
        mime,
        filename,
        bytes,
        dataBase64,
        isImage: typeof item.isImage === 'boolean' ? item.isImage : mime.startsWith('image/'),
      });
    }
  }
  return {
    text,
    sessionId,
    draftId,
    messageId,
    files: Array.isArray(files) ? files : [],
    attachments,
  };
}

/** The `faktor.mutationMode` setting vocabulary (shadow-only). */
export type MutationModeSetting = '' | 'shadow';

/**
 * Strict parse of one raw `faktor.mutationMode` value. `''`/absent is the
 * inherit-daemon default; `"shadow"` names the daemon's only mutation mode
 * explicitly. The removed `direct_compat` value is refused with the typed
 * removal reason (the same vocabulary the daemon's own strict decode and
 * config parser use), and every unknown value is refused as unknown. The
 * function never coerces and never falls back silently.
 */
export function parseMutationModeSetting(
  raw: unknown,
): { readonly mode: MutationModeSetting } | { readonly reason: string } {
  if (raw === undefined || raw === null || raw === '') {
    return { mode: '' };
  }
  if (raw === 'shadow') {
    return { mode: 'shadow' };
  }
  if (raw === 'direct_compat') {
    return {
      reason:
        'mutation_mode "direct_compat" was removed: every mutating run executes in an ' +
        'isolated candidate (shadow mutation); there is no direct-owner mode. Remove the ' +
        'setting or set "faktor.mutationMode" to "shadow".',
    };
  }
  return {
    reason: `unknown mutation mode ${JSON.stringify(raw)}: shadow mutation is the only mode`,
  };
}

/** Bounds of one composer attachment list (mirror the daemon's own caps). */
export const MAX_WEBVIEW_FILES = 64;
export const MAX_WEBVIEW_FILE_CHARS = 4096;

/** One refused composer attachment (kept, never a whole-message drop). */
export interface WebviewFileRefusal {
  readonly index: number;
  readonly reason: string;
}

/**
 * Bounded composer attachment mapping. Every malformed entry is refused
 * individually with a reason (never echoed unbounded), the containing
 * message is never dropped wholesale because of it, and the goal always
 * survives. Paths are structural only: the daemon re-validates against the
 * workspace before use.
 */
export function boundedWebviewFiles(raw: unknown): {
  readonly files: string[];
  readonly refused: readonly WebviewFileRefusal[];
} {
  const files: string[] = [];
  const refused: WebviewFileRefusal[] = [];
  if (raw === undefined || raw === null) {
    return { files, refused };
  }
  if (!Array.isArray(raw)) {
    return { files, refused: [{ index: 0, reason: 'files must be an array of attachment paths' }] };
  }
  const refuse = (index: number, reason: string): void => {
    if (refused.length < MAX_WEBVIEW_FILES) {
      refused.push({ index, reason });
    }
  };
  for (let index = 0; index < raw.length; index += 1) {
    const entry = raw[index];
    if (typeof entry !== 'string') {
      refuse(index, 'attachment path must be a string');
      continue;
    }
    const trimmed = entry.trim();
    if (trimmed.length === 0) {
      refuse(index, 'attachment path is empty');
      continue;
    }
    if (trimmed.length > MAX_WEBVIEW_FILE_CHARS) {
      refuse(index, `attachment path exceeds ${MAX_WEBVIEW_FILE_CHARS} characters`);
      continue;
    }
    let control = false;
    for (let i = 0; i < trimmed.length; i += 1) {
      const code = trimmed.charCodeAt(i);
      if (code < 0x20 || code === 0x7f) {
        control = true;
        break;
      }
    }
    if (control) {
      refuse(index, 'attachment path carries control characters');
      continue;
    }
    if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(trimmed)) {
      // `data:`/`file:`/`vscode-remote:` bytes stay out of the native run:
      // the DTO carries workspace-relative paths only. Surfaced, not silent.
      refuse(index, 'attachment url schemes do not reach the native run (workspace-relative paths only)');
      continue;
    }
    if (trimmed.startsWith('/') || /^[a-zA-Z]:[\\/]/.test(trimmed) || trimmed.startsWith('\\\\')) {
      refuse(index, 'attachment path must be workspace-relative');
      continue;
    }
    const segments = trimmed.split(/[\\/]/);
    if (segments.some((segment) => segment === '..')) {
      refuse(index, 'attachment path traverses outside the workspace');
      continue;
    }
    if (files.length >= MAX_WEBVIEW_FILES) {
      refuse(index, `more than ${MAX_WEBVIEW_FILES} file attachments`);
      continue;
    }
    files.push(trimmed);
  }
  return { files, refused };
}

/**
 * Strict completion-contract parse for the host path. `{contract:null}` for
 * an absent value or the all-false default (today's path); `{contract}` for
 * a valid non-default contract; `{reason}` for anything malformed — the
 * caller must refuse the START loudly rather than silently run the task
 * contract-free, which would claim a workflow the run never recorded.
 */
export function parseCompletionContract(
  raw: unknown,
): { readonly contract: NativeCompletionContract | null } | { readonly reason: string } {
  if (raw === undefined || raw === null) {
    return { contract: null };
  }
  if (typeof raw !== 'object' || Array.isArray(raw)) {
    return { reason: 'completionContract must be an object' };
  }
  const record = raw as Record<string, unknown>;
  for (const key of Object.keys(record)) {
    if (key !== 'include_commit' && key !== 'include_push' && key !== 'include_pr') {
      return { reason: `completionContract.${key} is not a known member` };
    }
  }
  for (const key of ['include_commit', 'include_push', 'include_pr'] as const) {
    if (!Object.prototype.hasOwnProperty.call(record, key) || typeof record[key] !== 'boolean') {
      return { reason: `completionContract.${key} must be a boolean` };
    }
  }
  const contract: NativeCompletionContract = {
    include_commit: record.include_commit as boolean,
    include_push: record.include_push as boolean,
    include_pr: record.include_pr as boolean,
  };
  return hasCompletionSteps(contract) ? { contract } : { contract: null };
}

export interface StartTaskSettings {
  /** The raw `faktor.mutationMode` value; parsed strictly at admission. */
  readonly mutationMode: string;
  readonly maxTokens: number;
  /** Exact micro-USD budget (0n = daemon default); never a rounded float. */
  readonly maxCostMicro: MicroMoney;
  /** Workspace-relative attachment paths forwarded from the composer. */
  readonly files?: readonly string[];
  /** Durable typed binary attachments (uploaded BEFORE this start). */
  readonly attachments?: readonly TaskAttachmentId[];
  /** The Task-mode completion contract (null / all-false = default path). */
  readonly completionContract?: NativeCompletionContract | null;
}

/** The typed classification of a refused task start. */
export type StartFailureKind =
  | 'shadow_unregistered'
  | 'validation'
  | 'auth'
  | 'server'
  | 'transport';

export interface StartFailure {
  readonly kind: StartFailureKind;
  readonly status: number | null;
  readonly code: string | null;
  /** User-facing, actionable, and explicit about the opt-in. */
  readonly message: string;
}

export interface StartTaskOutcome {
  readonly ok: boolean;
  readonly runId: string | null;
  readonly started: NativeTaskRunStarted | null;
  readonly failure: StartFailure | null;
}

export interface StartRunClient {
  startTaskRun(sessionId: string, request: StartTaskRunRequest): Promise<NativeTaskRunStarted>;
}

/**
 * Strictly parse one completion contract from a trusted-UI message. Only
 * the three documented boolean members are accepted; anything else (a
 * missing member, a typed string, an extra member, a non-object) is
 * refused as `null` — never coerced, never partially applied. An all-false
 * contract is the default behavior and returns `null` (no wire field).
 *
 * Callers that must distinguish "no contract" from "malformed" (and refuse
 * the start loudly) use `parseCompletionContract`; this wrapper preserves
 * the original `null`-on-everything-invalid contract for callers that
 * treat both as the default path.
 */
export function completionContractSetting(raw: unknown): NativeCompletionContract | null {
  const parsed = parseCompletionContract(raw);
  return 'reason' in parsed ? null : parsed.contract;
}

/** TRUE when the contract requests at least one conditional step. */
export function hasCompletionSteps(contract: NativeCompletionContract | null): boolean {
  return (
    contract !== null &&
    (contract.include_commit || contract.include_push || contract.include_pr)
  );
}

/**
 * Build the strict request body. The empty setting (inherit-daemon) OMITS
 * `mutation_mode` entirely; only the explicit `"shadow"` value is sent.
 * The removed `direct_compat` value and any unknown value are dropped here
 * (the start admission path refuses them loudly BEFORE the wire); the
 * request builder never coerces a removed mode into a fabricated payload.
 *
 * A NON-DEFAULT completion contract is refused by the daemon on the plain
 * prompt path, so the request pairs it with ONE explicit mutating work item
 * (`main`) — the same in-session drive the plain prompt uses, with the
 * durable contract seam. The default path stays byte-identical (no
 * contract, no work item).
 */
export function startTaskRequest(goal: string, settings: StartTaskSettings): StartTaskRunRequest {
  const contract = hasCompletionSteps(settings.completionContract ?? null)
    ? (settings.completionContract as NativeCompletionContract)
    : null;
  const request: StartTaskRunRequest = {
    goal,
    ...(settings.maxTokens > 0 ? { max_tokens: settings.maxTokens } : {}),
    ...(settings.maxCostMicro > 0n
      ? { max_cost_micro: microWireValue(settings.maxCostMicro) }
      : {}),
    ...(settings.files !== undefined && settings.files.length > 0
      ? { files: settings.files }
      : {}),
    ...(settings.attachments !== undefined && settings.attachments.length > 0
      ? { attachments: settings.attachments }
      : {}),
    ...(contract !== null
      ? {
          work_items: [
            {
              id: 'main',
              kind: 'Implementation',
              summary: goal,
              ownership: 'isolated_worktree',
            },
          ],
          completion_contract: contract,
        }
      : {}),
  };
  const mode = parseMutationModeSetting(settings.mutationMode);
  if ('mode' in mode && mode.mode === 'shadow') {
    return { ...request, mutation_mode: 'shadow' };
  }
  return request;
}

function failureOf(error: unknown): StartFailure {
  if (error instanceof NativeApiError) {
    if (error.status === 409) {
      return {
        kind: 'shadow_unregistered',
        status: error.status,
        code: error.code,
        message:
          'cannot start task: the daemon refused the shadowed run because the session is ' +
          'not registered in the daemon worktree registry (shadow mutation needs a ' +
          `registered workspace/worktree). Server said: ${error.message}. Restart the ` +
          'daemon so a registered session is created, then retry.',
      };
    }
    const kind: StartFailureKind =
      error.status === 400
        ? 'validation'
        : error.status === 401 || error.status === 403
          ? 'auth'
          : 'server';
    return { kind, status: error.status, code: error.code, message: `cannot start task: ${error.message}` };
  }
  const message = error instanceof Error ? error.message : String(error);
  return {
    kind: 'transport',
    status: null,
    code: null,
    message: `cannot start task: ${message}`,
  };
}

/**
 * Start ONE task run: exactly one request, no downgrade retry. The outcome
 * is always returned (never thrown) so callers can ack the composer and
 * report the error deterministically. A removed/unknown mutation mode is a
 * typed `validation` refusal BEFORE any request: the removed value never
 * reaches the wire and is never silently ignored.
 */
export async function startTaskRun(input: {
  readonly client: StartRunClient;
  readonly sessionId: string;
  readonly goal: string;
  readonly settings: StartTaskSettings;
  readonly onStarted: (started: NativeTaskRunStarted) => void;
  readonly onFailure: (failure: StartFailure) => void;
}): Promise<StartTaskOutcome> {
  const mode = parseMutationModeSetting(input.settings.mutationMode);
  if ('reason' in mode) {
    const failure: StartFailure = {
      kind: 'validation',
      status: null,
      code: 'invalid_mutation_mode',
      message: `cannot start task: ${mode.reason}`,
    };
    input.onFailure(failure);
    return { ok: false, runId: null, started: null, failure };
  }
  const request = startTaskRequest(input.goal, input.settings);
  try {
    const started = await input.client.startTaskRun(input.sessionId, request);
    input.onStarted(started);
    return { ok: true, runId: started.run_id, started, failure: null };
  } catch (error) {
    const failure = failureOf(error);
    input.onFailure(failure);
    return { ok: false, runId: null, started: null, failure };
  }
}

// ------------------------------------------------- pending submissions

/** The upload half of the native client the admission flow needs. */
export interface AttachmentUploadClient {
  uploadAttachment(
    sessionId: string,
    request: { mime: string; filename?: string | null; data_base64: string },
  ): Promise<TaskAttachmentId>;
}

export type AdmitFailureStage = 'upload' | 'start';

/**
 * One admission failure. `kind` `image_unsupported` is the LOUD refusal of
 * an image submission while provider media/content parts are not wired;
 * `upload` covers a refused/failed byte upload; the remaining kinds are the
 * task-start classifications ([`StartFailureKind`]).
 */
export interface AdmitFailure {
  readonly kind: StartFailureKind | 'upload' | 'image_unsupported';
  readonly stage: AdmitFailureStage;
  readonly status: number | null;
  readonly code: string | null;
  readonly message: string;
}

export interface AdmitOutcome {
  readonly ok: boolean;
  readonly runId: string | null;
  /** The durable typed ids the run was admitted with (empty on failure). */
  readonly attachmentIds: readonly string[];
  readonly failure: AdmitFailure | null;
}

function uploadFailureOf(error: unknown, sessionId: string): AdmitFailure {
  if (error instanceof NativeApiError) {
    return {
      kind: 'upload',
      stage: 'upload',
      status: error.status,
      code: error.code,
      message: `cannot upload attachment to session ${sessionId}: ${error.message}`,
    };
  }
  const message = error instanceof Error ? error.message : String(error);
  return {
    kind: 'upload',
    stage: 'upload',
    status: null,
    code: null,
    message: `cannot upload attachment to session ${sessionId}: ${message}`,
  };
}

/**
 * Admit ONE pending submission through the daemon: upload every binary
 * attachment FIRST (images are refused loudly without uploading — provider
 * media/content parts are not wired), then start ONE task run carrying the
 * durable typed ids. The pending envelope is retained by the caller for the
 * entire call; `restore` is invoked EXACTLY ONCE on any failure (upload,
 * image refusal, validation/model/conflict/transport start failure) and
 * never on success — the draft is restored through the composer contract,
 * never silently lost. A failure before the start request leaves no
 * daemon-side admission at all.
 */
export async function admitPendingSubmission(input: {
  readonly client: StartRunClient & AttachmentUploadClient;
  readonly sessionId: string;
  readonly pending: PendingSubmission;
  readonly settings: Omit<StartTaskSettings, 'attachments'>;
  readonly onStarted: (started: NativeTaskRunStarted) => void;
  readonly onFailure: (failure: AdmitFailure) => void;
  readonly restore: (failure: AdmitFailure) => void;
}): Promise<AdmitOutcome> {
  const uploaded: TaskAttachmentId[] = [];
  // Pre-scan: one image refuses the WHOLE submission BEFORE any upload, so
  // a submission that can never reach a model leaves no partial bytes in
  // the durable store.
  const image = input.pending.attachments.find(
    (attachment) => attachment.isImage || attachment.mime.startsWith('image/'),
  );
  if (image !== undefined) {
    const failure: AdmitFailure = {
      kind: 'image_unsupported',
      stage: 'upload',
      status: 400,
      code: 'unsupported',
      message:
        'image attachments cannot be submitted: provider media/content parts are not wired, so the bytes can never reach a model; the draft and images were kept — retry without the image',
    };
    input.onFailure(failure);
    input.restore(failure);
    return { ok: false, runId: null, attachmentIds: [], failure };
  }
  for (const attachment of input.pending.attachments) {
    try {
      const id = await input.client.uploadAttachment(input.sessionId, {
        mime: attachment.mime,
        filename: attachment.filename,
        data_base64: attachment.dataBase64,
      });
      uploaded.push(id);
    } catch (error) {
      const failure = uploadFailureOf(error, input.sessionId);
      input.onFailure(failure);
      input.restore(failure);
      return { ok: false, runId: null, attachmentIds: uploaded.map((id) => id.digest), failure };
    }
  }
  const startRun = await startTaskRun({
    client: input.client,
    sessionId: input.sessionId,
    goal: input.pending.text,
    settings: { ...input.settings, attachments: uploaded },
    onStarted: input.onStarted,
    onFailure: (failure) => {
      input.onFailure({ ...failure, stage: 'start' });
    },
  });
  if (startRun.ok) {
    return {
      ok: true,
      runId: startRun.runId,
      attachmentIds: uploaded.map((id) => id.digest),
      failure: null,
    };
  }
  const startFailure = startRun.failure as StartFailure;
  const failure: AdmitFailure = {
    kind: startFailure.kind,
    stage: 'start',
    status: startFailure.status,
    code: startFailure.code,
    message: startFailure.message,
  };
  input.restore(failure);
  return {
    ok: false,
    runId: null,
    attachmentIds: uploaded.map((id) => id.digest),
    failure,
  };
}

