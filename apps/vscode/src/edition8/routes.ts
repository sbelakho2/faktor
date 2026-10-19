// Exact daemon route for every Revision 8 control.
//
// The routes below are the real routes registered by
// crates/server/src/api/lifecycle.rs and crates/server/src/native/*. A
// control whose capability the daemon does not serve is explicit `missing`
// with the precise route/DTO that is absent; it is never wired to a guessed
// path and never rendered as an effective/saved/value state.

import type { E8Snapshot, Json } from './contract.ts';
import { arrayAt, isObject } from './contract.ts';

export type E8RouteKind = 'config' | 'read' | 'action' | 'host' | 'missing';

export interface E8Route {
  readonly kind: E8RouteKind;
  readonly method: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
  readonly path: string;
  readonly configKey: string | null;
  readonly reason: string | null;
  readonly mutating: boolean;
}

function route(
  kind: E8RouteKind,
  method: E8Route['method'],
  path: string,
  configKey: string | null = null,
  reason: string | null = null,
): E8Route {
  return {
    kind,
    method,
    path,
    configKey,
    reason,
    mutating: kind === 'config' || kind === 'action',
  };
}

function cfg(key: string): E8Route {
  return route('config', 'POST', '/native/config/commit', key, null);
}

function read(path: string): E8Route {
  return route('read', 'GET', path, null, null);
}

function act(method: E8Route['method'], path: string): E8Route {
  return route('action', method, path, null, null);
}

function host(reason: string): E8Route {
  return route('host', 'GET', 'host', null, reason);
}

const ROUTES: Readonly<Record<string, E8Route>> = {
  // Models and routing
  'M-01': cfg('faktor.defaultProvider'),
  'M-02': cfg('faktor.defaultModel'),
  'M-03': host('Model favorites and recents are local MRU state; no daemon write exists and none is invented.'),
  'M-04': read('/native/capabilities'),
  'M-05': act('POST', '/native/presets/:id/roles'),
  'M-06': act('POST', '/native/presets/:id/fallback'),
  'M-07': read('/native/models'),
  'M-08': act('POST', '/native/catalog/probe'),
  'M-09': cfg('faktor.model.contextLimit'),
  'M-10': read('/native/capabilities'),
  'M-11': read('/native/call-receipts?session=:session&limit=50'),
  'M-12': read('/native/config/provenance?limit=500'),
  'M-13': read('/native/usage'),
  'M-14': act('POST', '/native/session/:session/tournament'),

  // Providers and local inference
  'P-01': act('POST', '/native/providers/connections'),
  'P-02': act('POST', '/native/providers/connections'),
  'P-03': act('POST', '/native/providers/connections'),
  'P-04': act('POST', '/native/providers/connections'),
  'P-05': act('POST', '/native/providers/connections'),
  'P-06': act('POST', '/native/providers/connections'),
  'P-07': act('POST', '/native/providers/connections'),
  'P-08': cfg('faktor.sandbox.allowLoopback'),
  'P-09': cfg('faktor.sandbox.network'),
  'P-10': act('POST', '/native/providers/connections/:id/test'),
  'P-11': act('POST', '/native/providers/connections'),
  'P-12': act('POST', '/native/providers/connections'),
  'P-13': read('/native/runtime/status'),
  'P-14': act('POST', '/native/providers/credentials/rotate'),
  'P-15': act('POST', '/native/providers/connections'),

  // Generation tuning (capability-gated compile)
  'G-01': act('POST', '/native/model-parameters/compile'),
  'G-02': act('POST', '/native/model-parameters/compile'),
  'G-03': cfg('faktor.generation.maxOutput'),
  'G-04': cfg('faktor.generation.temperature'),
  'G-05': cfg('faktor.generation.topP'),
  'G-06': act('POST', '/native/model-parameters/compile'),
  'G-07': act('POST', '/native/model-parameters/compile'),
  'G-08': act('POST', '/native/model-parameters/compile'),
  'G-09': act('POST', '/native/model-parameters/compile'),
  'G-10': act('POST', '/native/model-parameters/compile'),
  'G-11': act('POST', '/native/model-parameters/compile'),
  'G-12': act('POST', '/native/model-parameters/compile'),
  'G-13': act('POST', '/native/model-parameters/compile'),
  'G-14': act('POST', '/native/model-parameters/compile'),

  // Agent presets and delegation
  'A-01': read('/native/presets?limit=200'),
  'A-02': act('POST', '/native/presets'),
  'A-03': act('POST', '/native/presets'),
  'A-04': act('POST', '/native/presets'),
  'A-05': act('POST', '/native/presets'),
  'A-06': read('/native/presets?limit=200'),
  'A-07': act('POST', '/native/presets'),
  'A-08': act('POST', '/native/presets'),
  'A-09': act('POST', '/native/agents/:child/budget'),
  'A-10': act('POST', '/native/agents/:child/pause'),
  'A-11': read('/native/agents'),
  'A-12': read('/native/presets?limit=200'),

  // Context
  'C-01': read('/native/context/manifests?session=:session'),
  'C-02': read('/native/context/manifests?session=:session'),
  'C-03': act('POST', '/native/session/:session/context/preview'),
  'C-04': host('Current editor selection is a host read; the exact URI/range/version is captured by the native host action.'),
  'C-05': act('POST', '/native/session/:session/attachments'),
  'C-06': act('POST', '/native/session/:session/attachments'),
  'C-07': act('POST', '/native/session/:session/context/pins'),
  'C-08': act('POST', '/native/config/commit'),
  'C-09': cfg('faktor.retrieval.strategy'),
  'C-10': cfg('faktor.retrieval.embeddingProvider'),
  'C-11': read('/native/index/coverage?session=:session'),
  'C-12': act('POST', '/native/session/:session/context/preview'),
  'C-13': read('/native/context/manifest?session=:session'),
  'C-14': read('/native/context/export?session=:session'),

  // Compaction, memory and efficiency
  'K-01': cfg('faktor.context.compactionMinReduction'),
  'K-02': act('POST', '/native/session/:session/context/compaction'),
  'K-03': act('POST', '/native/session/:session/context/compaction'),
  'K-04': read('/native/session/:session/context/compaction'),
  'K-05': cfg('faktor.efficiency.failureLearning'),
  'K-06': cfg('faktor.efficiency.ccr'),
  'K-07': cfg('faktor.efficiency.typedHandoff'),
  'K-08': cfg('faktor.efficiency.semanticContext'),
  'K-09': cfg('faktor.efficiency.reworkGuard'),
  'K-10': cfg('faktor.memory.captureMode'),
  'K-11': act('DELETE', '/native/memory/records/:record?session=:session&expectedVersion=:version'),
  'K-12': read('/native/session/:session/efficiency'),

  // Instructions and prompt composition
  'I-01': act('POST', '/native/instructions/records'),
  'I-02': act('POST', '/native/instructions/records'),
  'I-03': act('POST', '/native/instructions/records'),
  'I-04': read('/native/instructions/records?limit=200'),
  'I-05': act('POST', '/native/instructions/records'),
  'I-06': read('/native/instructions/records?kind=template&limit=200'),
  'I-07': read('/native/instructions/records/preview'),
  'I-08': act('POST', '/native/instructions/import'),
  'I-09': read('/native/instructions/preview'),
  'I-10': act('POST', '/native/instructions/records'),
  'I-11': read('/native/instructions/bindings'),

  // Permissions and sandbox
  'Q-01': cfg('faktor.permissions.preset'),
  'Q-02': cfg('faktor.permissions.allowedTools'),
  'Q-03': cfg('faktor.sandbox.shell'),
  'Q-04': cfg('faktor.sandbox.shell'),
  'Q-05': cfg('faktor.sandbox.network'),
  'Q-06': act('POST', '/native/permission/reply'),
  'Q-07': act('POST', '/native/permissions/rules'),
  'Q-08': act('POST', '/native/permissions/preview'),
  'Q-09': act('DELETE', '/native/permissions/rules/:rule'),
  'Q-10': read('/native/permissions/policy'),
  'Q-11': host('Sandbox setup is a native host-controlled flow; the daemon publishes the fail-closed policy and the host opens its own setup guidance. No daemon write is invented.'),
  'Q-12': act('POST', '/native/enterprise/effective-config'),
  'Q-13': read('/native/permissions/audit?limit=50'),
  'Q-14': read('/native/permissions/policy'),

  // Tools and MCP
  'T-01': read('/native/tools'),
  'T-02': cfg('faktor.permissions.allowedTools'),
  'T-03': act('POST', '/native/mcp/servers'),
  'T-04': act('POST', '/native/mcp/servers'),
  'T-05': act('POST', '/native/mcp/servers'),
  'T-06': act('POST', '/native/mcp/servers/:mcp/auth'),
  'T-07': read('/native/mcp/servers'),
  'T-08': cfg('faktor.permissions.allowedTools'),
  'T-09': read('/native/mcp/servers/:mcp/tools'),
  'T-10': act('POST', '/native/mcp/servers'),
  'T-11': read('/native/tools'),
  'T-12': read('/native/mcp/servers/:mcp/resources'),
  'T-13': act('POST', '/native/mcp/servers/:mcp/start'),
  'T-14': act('POST', '/native/mcp/servers'),
  'T-15': read('/native/diagnostics/preview'),

  // Skills and hooks
  'S-01': read('/native/skills'),
  'S-02': act('POST', '/native/skills/:id/invocation'),
  'S-03': read('/native/skills/:skill'),
  'S-04': act('POST', '/native/skills/install'),
  'S-05': host('The slash-command palette inserts text into the host editor; it does not grant authority.'),
  'S-06': act('POST', '/native/instructions/records'),
  'S-07': act('POST', '/native/hooks'),
  'S-08': act('POST', '/native/hooks'),
  'S-09': act('POST', '/native/hooks'),
  'S-10': act('POST', '/native/hooks'),
  'S-11': act('POST', '/native/hooks'),
  'S-12': act('POST', '/native/hooks'),
  'S-13': act('POST', '/native/hooks/:hook/test'),
  'S-14': read('/native/skills'),

  // Terminal, browser and language tooling
  'X-01': read('/native/terminals'),
  'X-02': cfg('faktor.sandbox.shell'),
  'X-03': read('/native/session/:session/terminals/:terminal/output'),
  'X-04': act('POST', '/native/session/:session/terminal'),
  'X-05': act('POST', '/native/session/:session/terminals/:terminal/kill'),
  'X-06': act('POST', '/native/browser/sessions'),
  'X-07': act('POST', '/native/browser/validate-url'),
  'X-08': act('POST', '/native/browser/sessions/:browser/screenshot'),
  'X-09': read('/native/browser/sessions/:browser/observations'),
  'X-10': host('Language-server diagnostics are read from the host; no daemon route duplicates them.'),
  'X-11': host('Inline code actions are executed by the host editor; the daemon never mutates the editor.'),
  'X-12': cfg('faktor.sandbox.shell'),
  'X-13': read('/native/runtime/status'),

  // Budgets, retries and reliability
  'B-01': cfg('faktor.budget.maxTokens'),
  'B-02': cfg('faktor.budget.maxCostMicro'),
  'B-03': cfg('faktor.budget.warnAtPercent'),
  'B-04': read('/native/session/:session/usage'),
  'B-05': act('PUT', '/native/session/:session/budget'),
  'B-06': cfg('faktor.retry.maxAttempts'),
  'B-07': cfg('faktor.retry.baseDelayMs'),
  'B-08': read('/native/session/:session/efficiency'),
  'B-09': act('POST', '/native/config/commit'),
  'B-10': read('/native/config/descriptors'),
  'B-11': read('/native/health'),
  'B-12': read('/native/outcomes?limit=50'),

  // Changes, checkpoints and verification
  'V-01': read('/native/session/:session/verification'),
  'V-02': act('POST', '/native/session/:session/review/comments'),
  'V-03': act('POST', '/native/session/:session/changes/accept'),
  'V-04': read('/native/session/:session/checkpoints'),
  'V-05': act('POST', '/native/session/:session/checkpoints/:sequence/restore/preview'),
  'V-06': act('POST', '/native/session/:session/fork'),
  'V-07': act('POST', '/native/session/:session/task-runs'),
  'V-08': read('/native/session/:session/verification'),
  'V-09': read('/native/tasks/:task/proof?session=:session'),
  'V-10': act('POST', '/native/session/:session/task-runs'),
  'V-11': read('/native/repositories'),
  'V-12': act('POST', '/native/session/:session/checkpoints/:sequence/restore'),

  // Sessions, displays and accessibility
  'D-01': read('/native/sessions?limit=200'),
  'D-02': read('/native/sessions?query=:query&limit=200'),
  'D-03': read('/native/transcript/search?session=:session&q=:query&regex=false&case=false'),
  'D-04': act('POST', '/native/sessions/:session/meta'),
  'D-05': act('POST', '/native/session/:session/prompt'),
  'D-06': act('POST', '/native/session/:session/export'),
  'D-06-delete': act('DELETE', '/native/session/:session'),
  'D-07': read('/native/session/:session/events'),
  'D-08': host('Display focus/verbosity is host-local; it never changes daemon authority.'),
  'D-09': host('Theme, zoom and density belong to the VS Code host and are never overridden by Faktor.'),
  'D-10': host('Composer send behavior is the host keybinding.'),
  'D-11': host('Keymaps and command discovery are host-owned.'),
  'D-12': cfg('faktor.telemetry.categories'),
  'D-13': read('/native/session/:session/events'),
  'D-14': host('Accessibility and reduced-motion preferences are read from the host.'),
  'D-15': read('/native/sessions/parallel'),

  // Privacy, diagnostics, optional services and distribution
  'O-01': read('/native/config/search?q=:query'),
  'O-02': read('/native/runtime/status'),
  'O-03': act('POST', '/native/diagnostics/export'),
  'O-04': read('/native/runtime/status'),
  'O-05': act('POST', '/native/updater/check'),
  'O-06': read('/native/usage'),
  'O-07': read('/native/identity'),
  'O-08': act('POST', '/native/enterprise/effective-config'),
  'O-09': act('POST', '/native/memory/records/retention'),
  'O-10': cfg('faktor.telemetry.enabled'),
  'O-11': read('/native/workers'),
  'O-12': act('POST', '/native/config/import'),
  'O-13': read('/native/semantic/capabilities'),
};

export function rawRoute(controlId: string): E8Route {
  const entry = ROUTES[controlId];
  if (entry === undefined) {
    throw new Error(`edition8: no route for control ${controlId}`);
  }
  return entry;
}

export function missingControls(): readonly { id: string; method: string; path: string; reason: string }[] {
  return Object.entries(ROUTES)
    .filter(([, value]) => value.kind === 'missing')
    .map(([id, value]) => ({ id, method: value.method, path: value.path, reason: value.reason ?? '' }));
}

export function hostControls(): readonly string[] {
  return Object.entries(ROUTES)
    .filter(([, value]) => value.kind === 'host')
    .map(([id]) => id);
}

function firstConnectionId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.connections, 'connections')) {
    if (isObject(entry) && typeof entry.id === 'string') return entry.id;
  }
  return null;
}

function firstChildId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.agents, 'agents')) {
    if (!isObject(entry)) continue;
    if (typeof entry.id === 'string') return entry.id;
    if (typeof entry.child_id === 'string') return entry.child_id;
  }
  return null;
}

function firstTerminalId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.terminals, 'terminals')) {
    if (isObject(entry) && typeof entry.id === 'string') return entry.id;
  }
  for (const entry of arrayAt(snapshot.sessionUsage, 'terminals')) {
    if (isObject(entry) && typeof entry.id === 'string') return entry.id;
  }
  return null;
}

function firstTaskId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.tasks, 'tasks')) {
    if (isObject(entry) && typeof entry.id === 'string') return entry.id;
  }
  return null;
}

function firstSessionId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.sessions, 'sessions')) {
    if (isObject(entry) && typeof entry.id === 'string') return entry.id;
  }
  return null;
}

function firstMcpServerId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.mcp, "servers")) {
    if (!isObject(entry)) continue;
    if (typeof entry.id === "string") return entry.id;
    const record = entry.record;
    if (isObject(record) && typeof record.id === "string") return record.id;
  }
  return null;
}

function firstSkillId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.skills, "skills")) {
    if (isObject(entry) && typeof entry.id === "string") return entry.id;
  }
  return null;
}

function firstHookId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.hooks, "hooks")) {
    if (!isObject(entry)) continue;
    if (typeof entry.id === "string") return entry.id;
    const record = entry.record;
    if (isObject(record) && typeof record.id === "string") return record.id;
  }
  return null;
}

function firstMemoryRecordId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.memory, "records")) {
    if (isObject(entry) && typeof entry.id === "string") return entry.id;
  }
  for (const entry of arrayAt(snapshot.memory, "facts")) {
    if (isObject(entry) && typeof entry.id === "string") return entry.id;
  }
  return null;
}

function firstMemoryVersion(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.memory, "records")) {
    if (!isObject(entry)) continue;
    if (typeof entry.version === "number") return String(entry.version);
    if (typeof entry.version === "string") return entry.version;
  }
  return null;
}

function firstBrowserSessionId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.browserSessions, "sessions")) {
    if (isObject(entry) && typeof entry.id === "string") return entry.id;
  }
  return null;
}

function firstRuleId(snapshot: E8Snapshot): string | null {
  for (const entry of arrayAt(snapshot.policy, "rules")) {
    if (isObject(entry) && typeof entry.id === "string") return entry.id;
  }
  return null;
}

export interface ResolveContext {
  readonly payload: Readonly<Record<string, Json>>;
  readonly snapshot: E8Snapshot | null;
}

function payloadString(payload: Readonly<Record<string, Json>>, ...keys: string[]): string | null {
  for (const key of keys) {
    const value = payload[key];
    if (typeof value === 'string' && value.length > 0) return value;
  }
  return null;
}

/**
 * Resolve a route template against the staged payload and the served
 * snapshot. A missing identity is a typed refusal (never a placeholder id).
 */
export function resolveRoute(controlId: string, context: ResolveContext): { readonly route: E8Route; readonly unresolved: string | null } {
  const base = rawRoute(controlId);
  if (base.kind === 'config' || base.kind === 'host' || base.kind === 'missing') {
    return { route: base, unresolved: null };
  }
  const snapshot = context.snapshot;
  const payload = context.payload;
  const session =
    payloadString(payload, 'session') ??
    snapshot?.binding?.session ??
    null;
  const values: Record<string, string | null> = {
    session,
    id: payloadString(payload, 'id', 'connection') ?? (snapshot === null ? null : firstConnectionId(snapshot)),
    provider: payloadString(payload, 'provider', 'id') ?? null,
    model: payloadString(payload, 'model') ?? null,
    child: payloadString(payload, 'child', 'child_id', 'agent') ?? (snapshot === null ? null : firstChildId(snapshot)),
    terminal: payloadString(payload, 'terminal', 'terminal_id') ?? (snapshot === null ? null : firstTerminalId(snapshot)),
    task: payloadString(payload, 'task', 'task_id') ?? (snapshot === null ? null : firstTaskId(snapshot)),
    sequence: payloadString(payload, 'sequence', 'checkpoint') ?? null,
    kind: payloadString(payload, 'kind') ?? 'memory',
    key: payloadString(payload, 'key') ?? null,
    record: payloadString(payload, 'record', 'memory', 'id') ?? (snapshot === null ? null : firstMemoryRecordId(snapshot)),
    version: payloadString(payload, 'version', 'expectedVersion', 'expected_version') ?? (snapshot === null ? null : firstMemoryVersion(snapshot)),
    mcp: payloadString(payload, 'mcp', 'server', 'id') ?? (snapshot === null ? null : firstMcpServerId(snapshot)),
    skill: payloadString(payload, 'skill', 'id') ?? (snapshot === null ? null : firstSkillId(snapshot)),
    hook: payloadString(payload, 'hook', 'id') ?? (snapshot === null ? null : firstHookId(snapshot)),
    browser: payloadString(payload, 'browser', 'browserSession', 'id') ?? (snapshot === null ? null : firstBrowserSessionId(snapshot)),
    rule: payloadString(payload, 'rule', 'id') ?? (snapshot === null ? null : firstRuleId(snapshot)),
    query: payloadString(payload, 'query', 'q') ?? '',
  };
  const tokens = [...base.path.matchAll(/:([a-z_]+)/g)].map((match) => match[1]);
  for (const token of tokens) {
    if (token === 'query') continue;
    if (values[token] === null || values[token] === undefined) {
      return {
        route: base,
        unresolved: `No ${token} is selected or served; the ${controlId} call was not sent.`,
      };
    }
  }
  let path = base.path;
  for (const [token, value] of Object.entries(values)) {
    if (value === null) continue;
    path = path.replaceAll(`:${token}`, encodeURIComponent(value));
  }
  if (tokens.includes('query')) path = path.replace(':query', encodeURIComponent(values.query ?? ''));
  return { route: { ...base, path }, unresolved: null };
}

/** The route map is complete by construction; this returns holes loudly. */
export function assertRoutesComplete(controlIds: readonly string[]): readonly string[] {
  return controlIds.filter((id) => ROUTES[id] === undefined);
}

/** Human-readable route for UI detail rows. */
export function routeText(controlId: string): string {
  const entry = ROUTES[controlId];
  if (entry === undefined) return 'No route';
  if (entry.kind === 'config') return `config patch · ${entry.configKey ?? 'unknown key'}`;
  if (entry.kind === 'host') return 'host action';
  if (entry.kind === 'missing') return `missing · ${entry.method} ${entry.path}`;
  return `${entry.method} ${entry.path}`;
}

/** The exact missing-endpoint ledger used by the report/harness. */
export const E8_MISSING_ENDPOINTS = Object.freeze(missingControls());

/** Source of the first model pair, if served. */
export function selectedPair(snapshot: E8Snapshot | null): { provider: string; model: string } | null {
  if (snapshot === null || !Array.isArray(snapshot.models)) return null;
  for (const entry of snapshot.models) {
    if (!isObject(entry)) continue;
    const provider = entry.provider;
    const model = entry.model ?? entry.id;
    if (typeof provider === 'string' && typeof model === 'string') return { provider, model };
  }
  return null;
}

export function controlReadRoute(controlId: string): string {
  const entry = rawRoute(controlId);
  return `${entry.method} ${entry.path}`;
}

export function endpointSourceKey(controlId: string): string {
  switch (controlId) {
    case 'M-01':
    case 'M-02':
      return 'config';
    default:
      return 'health';
  }
}

export function routeKind(controlId: string): E8RouteKind {
  return rawRoute(controlId).kind;
}

export function routePath(controlId: string): string {
  return rawRoute(controlId).path;
}

export function routeMethod(controlId: string): string {
  return rawRoute(controlId).method;
}

export function routeConfigKey(controlId: string): string | null {
  return rawRoute(controlId).configKey;
}

export function routeReason(controlId: string): string | null {
  return rawRoute(controlId).reason;
}

export function routeSessionScoped(controlId: string): boolean {
  return rawRoute(controlId).path.includes(':session');
}

export function routeTemplate(controlId: string): string {
  return `${rawRoute(controlId).method} ${rawRoute(controlId).path}`;
}

export function isMutating(controlId: string): boolean {
  return rawRoute(controlId).mutating;
}

export function firstServedModel(snapshot: E8Snapshot | null): string | null {
  const pair = selectedPair(snapshot);
  return pair === null ? null : `${pair.provider}/${pair.model}`;
}

export function firstServedSession(snapshot: E8Snapshot | null): string | null {
  return snapshot === null ? null : firstSessionId(snapshot);
}
