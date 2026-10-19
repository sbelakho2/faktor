// Typed Edition 8 client: one composed read pass, one atomic configuration
// commit, and one exact native call per control. A refused route surfaces the
// daemon's typed API error; the panel renders that cause and keeps the last
// coherent state. Nothing here marks a state saved/verified on its own.

import { NativeApiError, NativeClient } from '../nativeClient.ts';
import {
  isObject,
  numberAt,
  type E8ApplyResult,
  type E8ConfigValue,
  type E8Snapshot,
  type Json,
} from './contract.ts';
import { bindingOf8 } from './bindings.ts';
import { resolveRoute } from './routes.ts';
import { configValues, loadEdition8Snapshot } from './source.ts';
import { contextDraftSources } from './hostDraft.ts';

export interface E8ReadOptions {
  readonly session: string | null;
  readonly workspace: string | null;
  readonly worktree: string | null;
  readonly daemon: string | null;
  readonly generation: number | null;
  readonly previous: E8Snapshot | null;
}

export interface E8Backend {
  readonly read: (options: E8ReadOptions) => Promise<E8Snapshot>;
  readonly applyPatch: (
    expectedRevision: string,
    changes: readonly { readonly key: string; readonly value: Json | undefined; readonly scope: string }[],
  ) => Promise<E8ApplyResult>;
  readonly execute: (
    controlId: string,
    payload: Readonly<Record<string, Json>>,
    snapshot: E8Snapshot | null,
  ) => Promise<E8ActionResult>;
}

export interface E8ActionResult {
  readonly ok: boolean;
  readonly message: string;
  readonly method: string;
  readonly path: string;
  readonly body: Json | null;
  readonly errorCode: string | null;
  readonly unresolved: string | null;
}

function textOf(value: Json | null, ...keys: string[]): string | null {
  if (!isObject(value)) return null;
  for (const key of keys) {
    const entry = value[key];
    if (typeof entry === 'string' && entry.length > 0) return entry;
  }
  return null;
}

function booleanOf(value: Json | null, key: string): boolean | null {
  if (!isObject(value)) return null;
  const entry = value[key];
  return typeof entry === 'boolean' ? entry : null;
}

function sanitizePayload(payload: Readonly<Record<string, Json>>): Record<string, Json> {
  const out: Record<string, Json> = {};
  for (const [key, value] of Object.entries(payload)) {
    if (key === 'control_id' || key === 'screen_id') continue;
    if (value === null || typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') {
      out[key] = value;
    } else if (Array.isArray(value) && value.every((entry) => typeof entry === 'string')) {
      out[key] = value;
    }
  }
  return out;
}

function selectedPair(snapshot: E8Snapshot | null): { provider: string; model: string } | null {
  if (snapshot === null || !Array.isArray(snapshot.models)) return null;
  for (const entry of snapshot.models) {
    if (!isObject(entry)) continue;
    const provider = entry.provider;
    const model = entry.model ?? entry.id;
    if (typeof provider === 'string' && typeof model === 'string') return { provider, model };
  }
  return null;
}

function generationKey(controlId: string): string {
  const map: Record<string, string> = {
    'G-01': 'reasoning',
    'G-02': 'thinking_budget',
    'G-03': 'max_output',
    'G-04': 'temperature',
    'G-05': 'top_p',
    'G-06': 'top_k',
    'G-07': 'frequency_penalty',
    'G-08': 'seed',
    'G-09': 'stop',
    'G-10': 'parallel_tool_calls',
    'G-11': 'output_format',
    'G-12': 'service_tier',
    'G-13': 'stream',
    'G-14': 'preview',
  };
  return map[controlId] ?? 'preview';
}

function bodyFor(controlId: string, payload: Readonly<Record<string, Json>>, snapshot: E8Snapshot | null): Json {
  const body = sanitizePayload(payload);
  const pair = selectedPair(snapshot);
  switch (controlId) {
    case 'M-14':
      return {
        goal: body.goal ?? '',
        criteria: Array.isArray(body.criteria) ? body.criteria : [],
        n: body.n ?? 2,
        model: body.model ?? (pair === null ? null : `${pair.provider}/${pair.model}`),
        maxTokens: body.maxTokens ?? null,
        maxCostMicro: body.maxCostMicro ?? null,
        mutationMode: body.mutationMode ?? 'ReadOnly',
        files: Array.isArray(body.files) ? body.files : [],
      };
    case 'P-01':
    case 'P-02':
    case 'P-03':
    case 'P-04':
    case 'P-05':
    case 'P-06':
    case 'P-07':
    case 'P-11':
    case 'P-12':
    case 'P-15': {
      const id = typeof body.id === 'string' ? body.id : typeof body.connection === 'string' ? body.connection : '';
      return {
        id,
        family: body.family ?? body.kind ?? 'openai',
        label: body.label ?? id,
        endpoint: body.endpoint ?? body.base_url ?? null,
        credential_ref: body.credential_ref ?? body.credentialRef ?? null,
        models: Array.isArray(body.models) ? body.models : [],
        allow_loopback: body.allow_loopback ?? body.allowLoopback ?? null,
        api_flavor: body.api_flavor ?? null,
        trust_origin: body.trust_origin ?? null,
        connect_timeout_ms: body.connect_timeout_ms ?? body.connectTimeoutMs ?? null,
        first_byte_timeout_ms: body.first_byte_timeout_ms ?? body.firstByteTimeoutMs ?? null,
        total_timeout_ms: body.total_timeout_ms ?? body.totalTimeoutMs ?? null,
        keep_alive: body.keep_alive ?? body.keepAlive ?? null,
        proxy_url: body.proxy_url ?? body.proxyUrl ?? null,
        tls_mode: body.tls_mode ?? body.tlsMode ?? null,
        ca_cert_ref: body.ca_cert_ref ?? body.caCertRef ?? null,
        expectedRevision: body.expectedRevision ?? null,
      };
    }
    case 'M-05':
      return {
        roleModels: isObject(body.roleModels) ? body.roleModels : {},
        expectedRevision: body.expectedRevision ?? null,
      };
    case 'M-06':
      return {
        fallbackChain: Array.isArray(body.fallbackChain) ? body.fallbackChain : [],
        expectedRevision: body.expectedRevision ?? null,
      };
    case 'M-08':
      return {
        connectionId: body.connectionId ?? body.id ?? body.connection ?? '',
        model: body.model ?? pair?.model ?? null,
      };
    case 'K-02': {
      const base = bodyFor('K-03', payload, snapshot);
      return { ...(isObject(base) ? base : {}), model: body.model ?? 'inherit' };
    }
    case 'T-06':
      return {
        action: body.action ?? 'connect',
        accessToken: body.accessToken ?? body.token ?? null,
        account: body.account ?? null,
        scopes: Array.isArray(body.scopes) ? body.scopes : [],
        expectedRevision: body.expectedRevision ?? null,
      };
    case 'T-10':
      return {
        id: body.id ?? null,
        name: body.name ?? '',
        command: body.command ?? '',
        args: Array.isArray(body.args) ? body.args : [],
        env: Array.isArray(body.env) ? body.env : [],
        trust_origin: body.trust_origin ?? null,
        trusted: body.trusted ?? null,
        enabled: body.enabled ?? null,
        transport: body.transport ?? 'stdio',
        endpoint: body.endpoint ?? null,
        timeout_ms: body.timeout_ms ?? body.timeoutMs ?? null,
        output_cap_bytes: body.output_cap_bytes ?? body.outputCapBytes ?? null,
        stderr_cap_bytes: body.stderr_cap_bytes ?? body.stderrCapBytes ?? null,
        expectedRevision: body.expectedRevision ?? null,
      };
    case 'V-02':
      return {
        id: body.id ?? null,
        taskId: body.taskId ?? body.task_id ?? null,
        changeId: body.changeId ?? body.change_id ?? null,
        path: body.path ?? '',
        lineStart: body.lineStart ?? body.line_start ?? 1,
        lineEnd: body.lineEnd ?? body.line_end ?? body.lineStart ?? body.line_start ?? 1,
        side: body.side ?? 'new',
        body: body.body ?? body.text ?? '',
        author: body.author ?? 'vscode',
        contentHash: body.contentHash ?? body.content_hash ?? null,
        resolved: body.resolved ?? false,
      };
    case 'V-03':
      return {
        approved: Array.isArray(body.approved) ? body.approved : [],
        rejected: Array.isArray(body.rejected) ? body.rejected : [],
      };
    case 'V-05':
      return { direction: body.direction ?? 'undo' };
    case 'D-06':
      return {
        redact: body.redact ?? true,
        maxBytes: body.maxBytes ?? null,
      };
    case 'P-10':
    case 'P-14':
      return {};
    case 'G-01':
    case 'G-02':
    case 'G-04':
    case 'G-05':
    case 'G-06':
    case 'G-07':
    case 'G-08':
    case 'G-09':
    case 'G-10':
    case 'G-11':
    case 'G-12':
    case 'G-13':
    case 'G-14': {
      const key = generationKey(controlId);
      const parameters: Record<string, Json> = {};
      if (body.value !== undefined && body.value !== null) parameters[key] = body.value;
      else if (isObject(body.parameters)) {
        for (const [entryKey, entryValue] of Object.entries(body.parameters)) parameters[entryKey] = entryValue;
      }
      return {
        provider: body.provider ?? pair?.provider ?? '',
        model: body.model ?? pair?.model ?? '',
        parameters,
        capabilityRevision: body.capabilityRevision ?? numberAt(snapshot?.parameters, 'capabilityRevision') ?? 0,
      };
    }
    case 'A-02':
    case 'A-03':
    case 'A-04':
    case 'A-05':
    case 'A-07':
    case 'A-08':
    case 'A-12': {
      const modes: Record<string, string> = { 'A-02': 'ask', 'A-03': 'plan' };
      const delegation = isObject(body.delegation) ? body.delegation : {};
      return {
        id: body.id ?? null,
        mode: body.mode ?? modes[controlId] ?? 'code',
        name: body.name ?? '',
        description: body.description ?? '',
        origin: body.origin ?? 'user',
        scope: body.scope ?? 'user',
        roleInstructions: body.roleInstructions ?? body.role_instructions ?? '',
        modelProvider: body.modelProvider ?? body.model_provider ?? pair?.provider ?? null,
        modelId: body.modelId ?? body.model_id ?? pair?.model ?? null,
        reasoningEffort: body.reasoningEffort ?? body.reasoning_effort ?? null,
        toolAllow: Array.isArray(body.toolAllow) ? body.toolAllow : [],
        toolDeny: Array.isArray(body.toolDeny) ? body.toolDeny : [],
        delegation: {
          maxDepth: delegation.maxDepth ?? 0,
          maxChildren: delegation.maxChildren ?? 0,
          parentBudgetSharePpm: delegation.parentBudgetSharePpm ?? 500000,
          allowedChildPresets: Array.isArray(delegation.allowedChildPresets) ? delegation.allowedChildPresets : [],
        },
        childConcurrency: body.childConcurrency ?? 0,
        budgetSharePpm: body.budgetSharePpm ?? 500000,
        expectedRevision: body.expectedRevision ?? null,
      };
    }
    case 'A-09':
      return {
        maxTokens: body.maxTokens ?? null,
        maxCostMicro: body.maxCostMicro ?? null,
      };
    case 'A-10':
      return { text: body.text ?? '' };
    case 'C-03':
    case 'C-12':
      return {
        operationId: body.operationId ?? body.operation_id ?? `ui-${controlId}-${Date.now()}`,
        callId: body.callId ?? null,
        model: body.model ?? pair?.model ?? null,
        capacity: body.capacity ?? null,
        input: body.input ?? null,
        outputReserve: body.outputReserve ?? null,
        safetyReserve: body.safetyReserve ?? null,
        basis: body.basis ?? null,
        sources: Array.isArray(body.sources) ? body.sources : [],
      };
    case 'C-05':
    case 'C-06':
      return {
        mime: body.mime ?? 'application/octet-stream',
        filename: body.filename ?? null,
        dataBase64: body.dataBase64 ?? body.data_base64 ?? '',
      };
    case 'C-07':
      return {
        sourceId: body.sourceId ?? body.source_id ?? '',
        action: body.action ?? 'pin',
      };
    case 'C-14':
      return null;
    case 'K-03':
      return {
        operationId: body.operationId ?? body.operation_id ?? `ui-compact-${Date.now()}`,
        targetTokens: body.targetTokens ?? body.target_tokens ?? null,
        minReductionRatio: body.minReductionRatio ?? body.min_reduction_ratio ?? null,
        maxHistoryMessages: body.maxHistoryMessages ?? body.max_history_messages ?? null,
        expectedRevision: body.expectedRevision ?? null,
        model: body.model ?? 'inherit',
      };
    case 'Q-06':
      return {
        session_id: body.session_id ?? body.session ?? snapshot?.binding?.session ?? '',
        permission_id: body.permission_id ?? body.request_id ?? body.id ?? '',
        decision: body.decision ?? 'deny',
      };
    case 'Q-07':
      return {
        id: body.id ?? null,
        scope: body.scope ?? 'user',
        tool_id: body.tool_id ?? body.tool ?? '',
        target: body.target ?? '',
        decision: body.decision ?? 'ask',
        expires_ms: body.expires_ms ?? null,
        expectedRevision: body.expectedRevision ?? numberAt(snapshot?.policy, 'revision') ?? 0,
      };
    case 'Q-08':
      return {
        tool_id: body.tool_id ?? body.tool ?? '',
        canonical_target: body.canonical_target ?? body.target ?? '',
        effect: body.effect ?? 'read',
      };
    case 'T-03':
    case 'T-04':
    case 'T-05':
      return {
        id: body.id ?? null,
        name: body.name ?? '',
        command: body.command ?? '',
        args: Array.isArray(body.args) ? body.args : [],
        env: Array.isArray(body.env) ? body.env : [],
        trust_origin: body.trust_origin ?? null,
        trusted: body.trusted ?? null,
        enabled: body.enabled ?? null,
        expectedRevision: body.expectedRevision ?? null,
      };
    case 'I-01':
    case 'I-02':
    case 'I-03':
    case 'I-05':
    case 'I-10':
    case 'S-06': {
      const kinds: Record<string, string> = { 'I-03': 'rule', 'I-10': 'output_style', 'S-06': 'template' };
      return {
        id: body.id ?? null,
        kind: body.kind ?? kinds[controlId] ?? 'instruction',
        title: body.title ?? '',
        content: body.content ?? body.text ?? '',
        origin: body.origin ?? (controlId === 'I-02' ? 'project' : 'user'),
        scope: body.scope ?? (controlId === 'I-02' ? 'project' : 'user'),
        activation: body.activation ?? 'always',
        globs: Array.isArray(body.globs) ? body.globs : [],
        enabled: body.enabled ?? true,
        expectedRevision: body.expectedRevision ?? null,
      };
    }
    case 'I-08':
      return {
        format: body.format ?? 'faktor-instructions/v1',
        bundle: body.bundle ?? null,
      };
    case 'I-09':
      return {
        prompt: body.prompt ?? '',
        touchedPaths: Array.isArray(body.touchedPaths) ? body.touchedPaths : [],
        selectedIds: Array.isArray(body.selectedIds) ? body.selectedIds : [],
      };
    case 'S-04':
      return {
        name: body.name ?? '',
        origin: body.origin ?? null,
        source_uri: body.source_uri ?? body.sourceUri ?? '',
        text: body.text ?? '',
        trusted: body.trusted ?? null,
        invocation: body.invocation ?? null,
        required_capabilities: Array.isArray(body.required_capabilities) ? body.required_capabilities : [],
      };
    case 'S-07':
    case 'S-08':
    case 'S-09':
    case 'S-10':
    case 'S-11':
    case 'S-12':
      return {
        id: body.id ?? `hook-${controlId}`,
        spec: body.spec ?? sanitizePayload(body),
        trusted: body.trusted ?? null,
        enabled: body.enabled ?? null,
      };
    case 'S-13':
      return { spec: body.spec ?? null };
    case 'D-04':
      return {
        title: body.title ?? null,
        pinned: body.pinned ?? null,
        archived: body.archived ?? null,
      };
    case 'D-05':
      return {
        session_id: body.session_id ?? body.session ?? snapshot?.binding?.session ?? '',
        submission_id: body.submission_id ?? body.submissionId ?? `ui-${Date.now()}`,
        prompt: body.prompt ?? body.text ?? '',
        files: Array.isArray(body.files) ? body.files : [],
      };
    case 'V-06':
    case 'V-12':
      return {
        expected_current_hash: body.expected_current_hash ?? body.expectedCurrentHash ?? null,
        sequence: body.sequence ?? null,
      };
    case 'V-07':
    case 'V-10':
      return sanitizePayload(body);
    case 'O-04':
      return {};
    case 'O-05':
      return {};
    case 'O-09':
      return {
        scope: body.scope ?? 'project',
        keep_days: body.keep_days ?? body.keepDays ?? null,
        dry_run: body.dry_run ?? true,
      };
    case 'O-12':
      return { bundle: body.bundle ?? null };
    case 'B-05':
      return {
        taskId: body.taskId ?? body.task_id ?? null,
        maxCostMicro: body.maxCostMicro ?? body.max_cost_micro ?? null,
        clear: body.clear ?? false,
      };
    case 'O-08':
    case 'Q-12':
      return { layers: Array.isArray(body.layers) ? body.layers : [] };
    case 'O-09':
      return {
        session: body.session ?? snapshot?.binding?.session ?? '',
        scope: body.scope ?? 'session',
        olderThanDays: body.olderThanDays ?? null,
        dryRun: body.dryRun ?? true,
        limit: body.limit ?? null,
      };
    case 'X-06':
      return {
        adapterId: body.adapterId ?? 'chromium-local',
        session: body.session ?? snapshot?.binding?.session ?? '',
        url: body.url ?? body.text ?? '',
      };
    case 'X-07':
      return { url: body.url ?? '' };
    case 'X-08':
      return {
        mime: body.mime ?? 'image/png',
        dataBase64: body.dataBase64 ?? body.data_base64 ?? '',
        inModelContext: body.inModelContext ?? false,
      };
    default:
      return body;
  }
}

export class Edition8Client implements E8Backend {
  constructor(private readonly native: NativeClient) {}

  async read(options: E8ReadOptions): Promise<E8Snapshot> {
    return loadEdition8Snapshot(this.native, {
      session: options.session,
      workspace: options.workspace,
      worktree: options.worktree,
      daemon: options.daemon,
      generation: options.generation,
      previous: options.previous,
    });
  }

  async applyPatch(
    expectedRevision: string,
    changes: readonly { readonly key: string; readonly value: Json | undefined; readonly scope: string }[],
  ): Promise<E8ApplyResult> {
    const revision = Number.parseInt(expectedRevision, 10);
    if (!Number.isFinite(revision) || revision < 0) {
      return {
        ok: false,
        message: `The served revision ${expectedRevision} is not a non-negative integer; no patch was sent.`,
        path: '/native/config/commit',
        body: null,
        error_code: 'invalid_revision',
        field_states: [],
      };
    }
    try {
      const raw = await this.native.edition5Json<Json>(
        'POST',
        '/native/config/commit',
        {
          expected_revision: revision,
          changes: changes.map((change) => ({
            key: change.key,
            scope: change.scope,
            ...(change.value === undefined ? {} : { value: change.value }),
          })),
        },
        (value) => value,
      );
      const saved = numberAt(raw, 'saved_revision');
      const runtime = numberAt(raw, 'runtime_revision');
      const fieldStates = isObject(raw) && isObject(raw.field_states)
        ? Object.entries(raw.field_states).map(([key, state]) => ({
            key,
            state: typeof state === 'string' ? state : JSON.stringify(state),
          }))
        : [];
      return {
        ok: saved !== null,
        message:
          saved === null
            ? 'The daemon did not return a saved revision; nothing is rendered saved.'
            : `Configuration revision ${saved} saved${runtime === saved ? '' : `; runtime revision ${runtime ?? 'unavailable'}`}.`,
        path: '/native/config/commit',
        body: raw,
        error_code: saved === null ? 'invalid_receipt' : null,
        field_states: fieldStates,
      };
    } catch (error) {
      return {
        ok: false,
        message: error instanceof NativeApiError ? `${error.code}: ${error.message}` : error instanceof Error ? error.message : String(error),
        path: '/native/config/commit',
        body: null,
        error_code: error instanceof NativeApiError ? error.code : 'transport',
        field_states: [],
      };
    }
  }

  async execute(
    controlId: string,
    payload: Readonly<Record<string, Json>>,
    snapshot: E8Snapshot | null,
  ): Promise<E8ActionResult> {
    const binding = bindingOf8(controlId);
    const resolved = resolveRoute(controlId, { payload, snapshot });
    const route = resolved.route;
    if (resolved.unresolved !== null) {
      return {
        ok: false,
        message: resolved.unresolved,
        method: route.method,
        path: route.path,
        body: null,
        errorCode: 'unresolved',
        unresolved: resolved.unresolved,
      };
    }
    if (route.kind === 'missing') {
      return {
        ok: false,
        message: route.reason ?? 'This capability is not served by the daemon.',
        method: route.method,
        path: route.path,
        body: null,
        errorCode: 'missing_endpoint',
        unresolved: null,
      };
    }
    if (route.kind === 'host') {
      return {
        ok: true,
        message: route.reason ?? 'Native host action requested.',
        method: 'HOST',
        path: 'host',
        body: null,
        errorCode: null,
        unresolved: null,
      };
    }
    if (route.kind === 'config') {
      const key = binding.configKey;
      if (key === null) {
        return {
          ok: false,
          message: 'This control has no configuration key; nothing was written.',
          method: 'POST',
          path: '/native/config/commit',
          body: null,
          errorCode: 'no_key',
          unresolved: null,
        };
      }
      const value = payload.value !== undefined ? payload.value : payload[controlId] !== undefined ? payload[controlId] : payload.set;
      const scope = typeof payload.scope === 'string' ? payload.scope : 'user';
      const unset = payload.unset === true || value === undefined && payload.useSample === true;
      const revision = snapshot?.sources.config.data !== null && isObject(snapshot?.config) ? String(snapshot.config.revision ?? '0') : '0';
      const result = await this.applyPatch(revision, [
        { key, value: unset ? undefined : (value as Json), scope },
      ]);
      return {
        ok: result.ok,
        message: result.message,
        method: 'POST',
        path: result.path,
        body: result.body,
        errorCode: result.error_code,
        unresolved: null,
      };
    }
    let body = route.kind === 'read' ? null : bodyFor(controlId, payload, snapshot);
    if (body !== null && isObject(body) && ['C-03', 'C-05', 'C-06', 'C-07', 'C-12', 'C-14'].includes(controlId)) {
      const sources = body.sources;
      if (!Array.isArray(sources) || sources.length === 0) {
        body = {
          ...body,
          sources: contextDraftSources().map((source) => ({
            kind: source.kind,
            uri: source.uri,
            range: source.range,
            version: source.version,
            reason: source.reason,
            tokens: null,
          })),
        };
      }
    }
    try {
      const raw =
        route.method === 'GET' && (route.path === '/models' || route.path === '/capabilities')
          ? await this.native.legacyCatalogJson('GET', route.path, (value) => value)
          : await this.native.edition5Json<Json>(route.method, route.path, body, (value) => value);
      const ok = booleanOf(raw, 'ok') ?? (booleanOf(raw, 'deleted') === true ? true : booleanOf(raw, 'applied') ?? true);
      return {
        ok,
        message:
          textOf(raw, 'message', 'state', 'status') ??
          (ok ? 'Daemon acknowledged the request.' : 'The daemon refused the request.'),
        method: route.method,
        path: route.path,
        body: raw,
        errorCode: ok ? null : 'refused',
        unresolved: null,
      };
    } catch (error) {
      return {
        ok: false,
        message: error instanceof NativeApiError ? `${error.code}: ${error.message}` : error instanceof Error ? error.message : String(error),
        method: route.method,
        path: route.path,
        body: null,
        errorCode: error instanceof NativeApiError ? error.code : 'transport',
        unresolved: null,
      };
    }
  }
}

/** Config lookup used by the panel and tests. */
export function servedConfigValue(snapshot: E8Snapshot | null, key: string): E8ConfigValue | null {
  if (snapshot === null) return null;
  return configValues(snapshot.config).find((entry) => entry.key === key) ?? null;
}
