// Typed Edition 5 client: one aggregate read, one atomic config patch, and
// one exact native call per control command. A refused route surfaces the
// daemon's typed API error; the panel renders that cause and keeps the last
// coherent state. Nothing here marks a state saved/verified on its own.

import { NativeApiError, NativeClient } from '../nativeClient.ts';
import { bindingOf, type E5CommandKind } from './bindings.ts';
import { commandCall, defaultPayload, READ_COMMANDS, type E5CommandCall, type E5Payload } from './commands.ts';
import {
  validateConfigSnapshot,
  validateProviderTest,
  validateSnapshot,
} from './validate.ts';
import type {
  E5ConfigPatchResult,
  E5ConfigSnapshot,
  E5ProviderTestResult,
  E5Snapshot,
  Json,
} from './contract.ts';

export interface E5ActionResult {
  readonly ok: boolean;
  readonly message: string;
  readonly call: E5CommandCall | null;
  readonly body: Json | null;
  readonly errorCode: string | null;
}

export interface E5Backend {
  readonly read: (session: string | null) => Promise<E5Snapshot>;
  readonly readConfig: () => Promise<E5ConfigSnapshot>;
  readonly applyPatch: (patch: {
    readonly expected_revision: string;
    readonly set: readonly { readonly key: string; readonly value: Json; readonly control_id: string }[];
    readonly unset: readonly string[];
  }) => Promise<E5ConfigPatchResult>;
  readonly execute: (command: E5CommandKind, payload: E5Payload) => Promise<E5ActionResult>;
  readonly testProvider: (payload: E5Payload) => Promise<E5ProviderTestResult>;
}

function anyJson(value: Json): Json {
  return value;
}

function messageOf(value: Json | null): string | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return null;
  const record = value as Record<string, Json>;
  for (const key of ['message', 'detail', 'reason']) {
    if (typeof record[key] === 'string' && (record[key] as string).length > 0) {
      return record[key] as string;
    }
  }
  return null;
}

function booleanField(value: Json | null, key: string): boolean | null {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) return null;
  const entry = (value as Record<string, Json>)[key];
  return typeof entry === 'boolean' ? entry : null;
}

function validatePatch(value: Json): E5ConfigPatchResult {
  if (value === null || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error('config patch: expected an object');
  }
  const record = value as Record<string, Json>;
  if (typeof record.revision !== 'string' || typeof record.applied !== 'boolean') {
    throw new Error('config patch: revision/applied missing');
  }
  if (!Array.isArray(record.field_states)) {
    throw new Error('config patch: field_states missing');
  }
  const fieldStates = record.field_states.map((entry) => {
    if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
      throw new Error('config patch: field state shape');
    }
    const row = entry as Record<string, Json>;
    if (typeof row.key !== 'string' || typeof row.state !== 'string') {
      throw new Error('config patch: field state key/state');
    }
    return { key: row.key, state: row.state as E5ConfigPatchResult['field_states'][number]['state'] };
  });
  const errors = Array.isArray(record.errors)
    ? record.errors.map((entry) => {
        if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
          throw new Error('config patch: error shape');
        }
        const row = entry as Record<string, Json>;
        if (typeof row.key !== 'string' || typeof row.code !== 'string' || typeof row.message !== 'string') {
          throw new Error('config patch: error fields');
        }
        return { key: row.key, code: row.code, message: row.message };
      })
    : [];
  return {
    revision: record.revision,
    applied: record.applied,
    field_states: fieldStates,
    errors,
    config: record.config == null ? null : validateConfigSnapshot(record.config, 'config'),
  };
}

export class Edition5Client implements E5Backend {
  constructor(private readonly native: NativeClient) {}

  async read(session: string | null): Promise<E5Snapshot> {
    const query = session === null ? '' : `?session=${encodeURIComponent(session)}`;
    const raw = await this.native.edition5Json('GET', `/native/ui/edition5${query}`, null, anyJson);
    return validateSnapshot(raw);
  }

  async readConfig(): Promise<E5ConfigSnapshot> {
    const raw = await this.native.edition5Json('GET', '/native/config/snapshot', null, anyJson);
    return validateConfigSnapshot(raw);
  }

  async applyPatch(patch: {
    readonly expected_revision: string;
    readonly set: readonly { readonly key: string; readonly value: Json; readonly control_id: string }[];
    readonly unset: readonly string[];
  }): Promise<E5ConfigPatchResult> {
    const raw = await this.native.edition5Json(
      'POST',
      '/native/config/patch',
      {
        expected_revision: patch.expected_revision,
        set: patch.set.map((entry) => ({ key: entry.key, value: entry.value, control_id: entry.control_id })),
        unset: patch.unset,
      } as unknown as Json,
      anyJson,
    );
    return validatePatch(raw);
  }

  async testProvider(payload: E5Payload): Promise<E5ProviderTestResult> {
    const call = commandCall('provider.test', payload);
    const raw = await this.native.edition5Json(call.method, call.path, call.body as Json | null, anyJson);
    return validateProviderTest(raw);
  }

  async execute(command: E5CommandKind, payload: E5Payload): Promise<E5ActionResult> {
    if (command === 'host.action') {
      return { ok: true, message: 'Native host action requested.', call: null, body: null, errorCode: null };
    }
    let call: E5CommandCall;
    try {
      call = commandCall(command, { ...defaultPayload(command, null), ...payload });
    } catch (error) {
      return {
        ok: false,
        message: error instanceof Error ? error.message : String(error),
        call: null,
        body: null,
        errorCode: 'missing_selection',
      };
    }
    try {
      const response = await this.native.edition5Json(call.method, call.path, call.body as Json | null, anyJson);
      const applied = booleanField(response, 'applied');
      const ok = booleanField(response, 'ok') ?? (applied === null ? true : applied);
      return {
        ok,
        message: messageOf(response) ?? this.ackText(command, response),
        call,
        body: response,
        errorCode: ok ? null : 'refused',
      };
    } catch (error) {
      return {
        ok: false,
        message:
          error instanceof NativeApiError
            ? `${error.code}: ${error.message}`
            : error instanceof Error
              ? error.message
              : String(error),
        call,
        body: null,
        errorCode: error instanceof NativeApiError ? error.code : 'transport',
      };
    }
  }

  /** Honest acknowledgement copy: never invents a saved/verified state. */
  private ackText(command: E5CommandKind, response: Json | null): string {
    if (READ_COMMANDS.has(command)) return 'Served state re-read.';
    const state = response === null || typeof response !== 'object' || Array.isArray(response)
      ? null
      : (response as Record<string, Json>).state;
    if (typeof state === 'string' && state.length > 0) {
      return `Daemon reported ${state}.`;
    }
    switch (command) {
      case 'approval.reply':
        return 'One exact waiter reply was acknowledged by the daemon.';
      case 'approval.remember':
        return 'A persistent rule draft was acknowledged; it replies to no pending request automatically.';
      case 'config.set':
      case 'settings.apply':
        return 'Configuration patch acknowledged.';
      case 'finish.request':
        return 'Finish request acknowledged; the outcome stays Pending until every required fact matches.';
      case 'restore.run':
        return 'Restore transaction acknowledged; conversation fork remains separate.';
      case 'fork.run':
        return 'Fork lineage acknowledged; file restore is a separate operation.';
      case 'policy.apply':
        return 'Policy version acknowledged for future dispatch.';
      case 'compaction.run':
        return 'Compaction operation acknowledged; inspect the measured result before claiming reduction.';
      case 'diagnostics.export':
        return 'Diagnostic export acknowledged; the artifact is redacted and bounded.';
      default:
        return 'Daemon acknowledged the request.';
    }
  }
}

/** True when a control's command needs a live session binding. */
export function commandNeedsSession(command: E5CommandKind): boolean {
  try {
    const call = commandCall(command, {
      session: 'session',
      id: 'id',
      preset: 'preset',
      role: 'role',
      source: 'source',
      operation: 'operation',
      rule: 'rule',
      request_id: 'request',
      tool: 'tool',
      skill: 'skill',
      hook: 'hook',
      terminal: 'terminal',
      agent: 'agent',
      path: 'path',
      checkpoint: 'checkpoint',
      item: 'item',
      command: 'command',
      cohort: 'all',
      period: 'all',
      worktree: '',
    });
    return call.path.includes('/native/session/');
  } catch {
    return false;
  }
}

export function bindingCommand(controlId: string): E5CommandKind {
  return bindingOf(controlId).command;
}
