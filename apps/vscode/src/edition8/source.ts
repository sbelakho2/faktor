// Composes the Edition 8 read model from the daemon's REAL routes.
//
// One bounded parallel read pass; every source is recorded with its exact
// method/path and either the served document or a typed error. Nothing is
// inferred from a missing source: a null section renders Unavailable and
// names the failed route. Session-scoped reads are skipped (recorded as
// unavailable, not empty) when the host has no bound session.

import { createHash } from 'node:crypto';
import { NativeApiError, NativeClient } from '../nativeClient.ts';
import {
  E8_SOURCE_KEYS,
  type E8ConfigSnapshot,
  type E8ConfigValue,
  type E8Snapshot,
  type E8Source,
  type E8Sources,
  type Json,
  isObject,
} from './contract.ts';

export interface E8SnapshotOptions {
  readonly session: string | null;
  readonly workspace: string | null;
  readonly worktree: string | null;
  readonly daemon: string | null;
  readonly generation: number | null;
  readonly previous: E8Snapshot | null;
  readonly now?: number;
}

function emptySource(method: 'GET' | 'POST', path: string, message: string): E8Source {
  return {
    state: 'unavailable',
    method,
    path,
    error: message,
    observed_at_ms: 0,
    data: null,
  };
}

function errorText(error: unknown): string {
  if (error instanceof NativeApiError) return `${error.code}: ${error.message}`;
  return error instanceof Error ? error.message : String(error);
}

/** Native routes wrap some singleton documents in their own named envelope. */
function unwrapEnvelope(value: Json | null, key: string): Json | null {
  if (isObject(value) && isObject(value[key])) return value[key];
  return value;
}

async function readJson(
  native: NativeClient,
  method: 'GET' | 'POST',
  path: string,
  body: Json | null,
  sources: Record<string, E8Source>,
  key: string,
): Promise<Json | null> {
  try {
    const data =
      path === '/models' || path === '/capabilities'
        ? await native.legacyCatalogJson('GET', path, (value) => value)
        : await native.edition5Json<Json>(method, path, body, (value) => value);
    sources[key] = {
      state: 'ready',
      method,
      path,
      error: null,
      observed_at_ms: Date.now(),
      data,
    };
    return data;
  } catch (error) {
    sources[key] = {
      state: 'error',
      method,
      path,
      error: errorText(error),
      observed_at_ms: Date.now(),
      data: null,
    };
    return null;
  }
}

function skip(sources: Record<string, E8Source>, key: string, method: 'GET' | 'POST', path: string, reason: string): null {
  sources[key] = emptySource(method, path, reason);
  return null;
}

function firstModel(models: Json | null): { provider: string; model: string } | null {
  if (!Array.isArray(models)) return null;
  for (const entry of models) {
    if (!isObject(entry)) continue;
    const provider = entry.provider;
    const model = entry.model ?? entry.id;
    if (typeof provider === 'string' && typeof model === 'string') return { provider, model };
  }
  return null;
}

function firstTask(tasks: Json | null): string | null {
  if (!Array.isArray(tasks)) return null;
  for (const entry of tasks) {
    if (isObject(entry) && typeof entry.id === 'string') return entry.id;
  }
  return null;
}

function firstRunTask(runs: Json | null): string | null {
  if (!Array.isArray(runs)) return null;
  for (const entry of runs) {
    if (!isObject(entry)) continue;
    if (typeof entry.task_id === 'number') return String(entry.task_id);
    if (typeof entry.task_id === 'string') return entry.task_id;
  }
  return null;
}

export interface E8ConfigLookup {
  readonly values: readonly E8ConfigValue[];
  readonly index: ReadonlyMap<string, E8ConfigValue>;
}

const CONFIG_LOOKUP = new WeakMap<object, E8ConfigLookup>();
const EMPTY_CONFIG_LOOKUP: E8ConfigLookup = { values: [], index: new Map() };

function configLookup(config: Json | null): E8ConfigLookup {
  if (!isObject(config)) return EMPTY_CONFIG_LOOKUP;
  const cached = CONFIG_LOOKUP.get(config);
  if (cached !== undefined) return cached;
  const values = readConfigValues(config);
  const lookup: E8ConfigLookup = {
    values,
    index: new Map(values.map((entry) => [entry.key, entry])),
  };
  CONFIG_LOOKUP.set(config, lookup);
  return lookup;
}

export function configValues(config: Json | null): readonly E8ConfigValue[] {
  return configLookup(config).values;
}

export function configValueMap(config: Json | null): ReadonlyMap<string, E8ConfigValue> {
  return configLookup(config).index;
}

function readConfigValues(config: Json | null): readonly E8ConfigValue[] {
  if (!isObject(config)) return [];
  const raw = config.values;
  const rows: E8ConfigValue[] = [];
  if (Array.isArray(raw)) {
    for (const entry of raw) {
      if (!isObject(entry) || typeof entry.key !== 'string') continue;
      rows.push({
        key: entry.key,
        value: entry.value ?? null,
        scope: typeof entry.scope === 'string' ? entry.scope : 'user',
        source: typeof entry.source === 'string' ? entry.source : 'unknown',
        locked: entry.locked === true,
        apply_state: typeof entry.apply_state === 'string' ? entry.apply_state : 'Unavailable',
        apply_timing: typeof entry.apply_timing === 'string' ? entry.apply_timing : 'unknown',
      });
    }
    return rows;
  }
  if (isObject(raw)) {
    for (const [key, entry] of Object.entries(raw)) {
      if (!isObject(entry)) continue;
      rows.push({
        key,
        value: entry.value ?? null,
        scope: typeof entry.scope === 'string' ? entry.scope : 'user',
        source: typeof entry.source === 'string' ? entry.source : 'unknown',
        locked: entry.locked === true,
        apply_state: typeof entry.apply_state === 'string' ? entry.apply_state : 'Effective',
        apply_timing: typeof entry.apply_timing === 'string' ? entry.apply_timing : 'unknown',
      });
    }
  }
  return rows;
}

function stableJson(value: Json): string {
  return JSON.stringify(value, (key, entry) => (key === 'snapshotMs' ? undefined : entry));
}

function dataRevision(
  sources: Record<string, E8Source>,
  session: string | null,
  options: E8SnapshotOptions,
): string {
  const hash = createHash('sha256');
  hash.update(session ?? '');
  hash.update('\0');
  hash.update(options.workspace ?? '');
  hash.update('\0');
  hash.update(options.worktree ?? '');
  hash.update('\0');
  hash.update(options.daemon ?? '');
  hash.update('\0');
  hash.update(String(options.generation ?? ''));
  for (const key of E8_SOURCE_KEYS) {
    const source = sources[key];
    if (source === undefined) {
      hash.update(`${key}\0missing\0`);
      continue;
    }
    hash.update(key);
    hash.update('\0');
    hash.update(source.state);
    hash.update('\0');
    if (source.data !== null) hash.update(stableJson(source.data));
    hash.update('\0');
  }
  return hash.digest('hex');
}

export function configSnapshot(config: Json | null): E8ConfigSnapshot | null {
  if (!isObject(config)) return null;
  const revision = config.revision;
  if (typeof revision !== 'number' && typeof revision !== 'string') return null;
  return {
    revision: String(revision),
    runtime_revision: String(config.runtime_revision ?? config.runtimeRevision ?? revision),
    values: configValues(config),
    secrets: [],
  };
}

/**
 * Load every Edition 8 source. Critical reads (health, ready, models, config)
 * fail closed: a failed critical read leaves its section null and the panel
 * marks the observation stale instead of showing an empty success.
 */
export async function loadEdition8Snapshot(native: NativeClient, options: E8SnapshotOptions): Promise<E8Snapshot> {
  const now = options.now ?? Date.now();
  const sources: Record<string, E8Source> = {};
  const session = options.session;
  const sessionPath = session === null ? null : `/native/session/${encodeURIComponent(session as string)}`;

  const critical = await Promise.all([
    readJson(native, 'GET', '/native/health', null, sources, 'health'),
    readJson(native, 'GET', '/native/ready', null, sources, 'ready'),
    readJson(native, 'GET', '/models', null, sources, 'models'),
    readJson(native, 'GET', '/capabilities', null, sources, 'capabilities'),
    readJson(native, 'GET', '/native/config/snapshot', null, sources, 'config'),
    skip(sources, 'descriptors', 'GET', '/native/config/descriptors', 'The effective config snapshot is authoritative; Revision 8 does not render the descriptor catalog.'),
    readJson(native, 'GET', '/native/providers/connections', null, sources, 'connections'),
    readJson(native, 'GET', '/native/providers', null, sources, 'providers'),
  ]);
  const [health, ready, models, capabilities, config, descriptors, connections, providers] = critical;
  // The commercial plane scopes usage/outcomes by organization. Identity is
  // read once; the two dependent reads start as soon as it lands while every
  // other independent read keeps running, so a commercial deployment adds no
  // extra serial round trip to screen open.
  const identityPromise = readJson(native, 'GET', '/native/identity', null, sources, 'identity').then(
    (value) => unwrapEnvelope(value, 'identity'),
  );
  const commercialPromise = identityPromise.then((identity) => {
    const organization =
      isObject(identity) && typeof identity.organization === 'string' && identity.organization.length > 0
        ? identity.organization
        : null;
    return {
      identity,
      usage: readJson(
        native,
        'GET',
        organization === null
          ? '/native/usage'
          : `/native/usage?org=${encodeURIComponent(organization)}&limit=1`,
        null,
        sources,
        'usage',
      ),
      outcomes: readJson(
        native,
        'GET',
        organization === null
          ? '/native/outcomes?limit=1'
          : `/native/outcomes?org=${encodeURIComponent(organization)}&limit=1`,
        null,
        sources,
        'outcomes',
      ),
    };
  });
  const selected = firstModel(models);
  const parametersPromise =
    selected === null
      ? Promise.resolve(
          skip(
            sources,
            'parameters',
            'GET',
            '/native/model-parameters',
            'No served model is available; the exact-model capability sheet cannot be read.',
          ),
        )
      : readJson(
          native,
          'GET',
          `/native/model-parameters?provider=${encodeURIComponent(selected.provider)}&model=${encodeURIComponent(selected.model)}`,
          null,
          sources,
          'parameters',
        );

  const otherPromise = Promise.all([
    sessionPath === null
      ? skip(sources, 'receipts', 'GET', '/native/call-receipts', 'No conversation is bound; call receipts are session-scoped.')
      : readJson(native, 'GET', `/native/call-receipts?session=${encodeURIComponent(session as string)}&limit=1`, null, sources, 'receipts'),
    readJson(native, 'GET', '/native/permissions/policy', null, sources, 'policy'),
    readJson(native, 'GET', '/native/permissions/audit?limit=50', null, sources, 'permissionAudit'),
    sessionPath === null
      ? skip(sources, 'approvals', 'GET', '/native/permissions', 'No conversation is bound; pending approvals are session-scoped.')
      : readJson(native, 'GET', `/native/permissions?session=${encodeURIComponent(session as string)}`, null, sources, 'approvals'),
    readJson(native, 'GET', '/native/mcp/servers', null, sources, 'mcp'),
    readJson(native, 'GET', '/native/tools?include_schema=false', null, sources, 'tools'),
    readJson(native, 'GET', '/native/skills', null, sources, 'skills'),
    readJson(native, 'GET', '/native/hooks', null, sources, 'hooks'),
    readJson(native, 'GET', '/native/hooks/audit?limit=1', null, sources, 'hookAudit'),
    sessionPath === null
      ? skip(sources, 'memory', 'GET', '/native/memory/records', 'No conversation is bound; memory is session/project-scoped.')
      : readJson(native, 'GET', `/native/memory/records?session=${encodeURIComponent(session as string)}&scope=all&limit=200`, null, sources, 'memory'),
    readJson(native, 'GET', '/native/sessions?limit=200', null, sources, 'sessions'),
    sessionPath === null
      ? skip(sources, 'sessionMeta', 'GET', '/native/sessions/{id}/meta', 'No conversation is bound; session metadata is session-scoped.')
      : readJson(native, 'GET', `/native/sessions/${encodeURIComponent(session as string)}/meta`, null, sources, 'sessionMeta'),
    commercialPromise.then((reads) => reads.usage),
    sessionPath === null
      ? skip(sources, 'sessionUsage', 'GET', '/native/session/{id}/usage', 'No conversation is bound; run usage is session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/usage`, null, sources, 'sessionUsage'),
    commercialPromise.then((reads) => reads.outcomes),
    sessionPath === null
      ? skip(sources, 'verification', 'GET', '/native/session/{id}/verification', 'No conversation is bound; verification is session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/verification`, null, sources, 'verification'),
    sessionPath === null
      ? skip(sources, 'checkpoints', 'GET', '/native/session/{id}/checkpoints', 'No conversation is bound; checkpoints are session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/checkpoints`, null, sources, 'checkpoints'),
    sessionPath === null
      ? skip(sources, 'taskRuns', 'GET', '/native/session/{id}/task-runs', 'No conversation is bound; task runs are session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/task-runs`, null, sources, 'taskRuns'),
    sessionPath === null
      ? skip(sources, 'tasks', 'GET', '/native/session/{id}/tasks', 'No conversation is bound; tasks are session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/tasks`, null, sources, 'tasks'),
    sessionPath === null
      ? skip(sources, 'turns', 'GET', '/native/session/{id}/turns', 'No conversation is bound; transcript turns are session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/turns`, null, sources, 'turns'),
    readJson(native, 'GET', '/native/diagnostics/preview', null, sources, 'diagnostics'),
    readJson(native, 'GET', '/native/updater/status', null, sources, 'updater'),
    commercialPromise.then((reads) => reads.identity),
    readJson(native, 'GET', '/native/entitlements', null, sources, 'entitlements'),
    readJson(native, 'GET', '/native/browser/status', null, sources, 'browser'),
    readJson(native, 'GET', '/native/enterprise/status', null, sources, 'enterprise'),
    readJson(native, 'GET', '/native/workers', null, sources, 'workers'),
    sessionPath === null
      ? skip(sources, 'index', 'GET', '/native/index/coverage', 'No conversation is bound; index coverage is session-scoped.')
      : readJson(native, 'GET', `/native/index/coverage?session=${encodeURIComponent(session as string)}`, null, sources, 'index'),
    sessionPath === null
      ? skip(sources, 'semantic', 'GET', '/native/semantic/status', 'Semantic retrieval status is read after a session binds.')
      : readJson(native, 'GET', '/native/semantic/status', null, sources, 'semantic'),
    sessionPath === null
      ? skip(sources, 'agents', 'GET', '/native/session/{id}/agents', 'No conversation is bound; agents are session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/agents`, null, sources, 'agents'),
    sessionPath === null
      ? skip(sources, 'board', 'GET', '/native/session/{id}/board', 'No conversation is bound; the coordination board is session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/board`, null, sources, 'board'),
    sessionPath === null
      ? skip(sources, 'tournaments', 'GET', '/native/session/{id}/tournaments', 'No conversation is bound; model comparisons are session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/tournaments`, null, sources, 'tournaments'),
    sessionPath === null
      ? skip(sources, 'terminals', 'GET', '/native/terminals', 'No conversation is bound; terminal ownership is session-scoped.')
      : readJson(native, 'GET', `/native/terminals?session=${encodeURIComponent(session as string)}`, null, sources, 'terminals'),
    readJson(native, 'GET', '/native/telemetry', null, sources, 'telemetry'),
  ]);

  const r8Promise = Promise.all([
    readJson(native, 'GET', '/native/presets?limit=20', null, sources, 'presets'),
    readJson(native, 'GET', '/native/instructions/records?limit=30', null, sources, 'instructions'),
    readJson(native, 'GET', '/native/runtime/status', null, sources, 'runtime'),
    readJson(native, 'GET', '/native/config/provenance?limit=40', null, sources, 'provenance'),
    readJson(native, 'GET', '/native/browser/adapters', null, sources, 'browserAdapters'),
    sessionPath === null
      ? skip(sources, 'browserSessions', 'GET', '/native/browser/sessions?session=', 'No conversation is bound; browser sessions are session-scoped.')
      : readJson(native, 'GET', `/native/browser/sessions?session=${encodeURIComponent(session as string)}`, null, sources, 'browserSessions'),
    sessionPath === null
      ? skip(sources, 'contextPins', 'GET', '/native/session/{id}/context/pins', 'No conversation is bound; context pins are session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/context/pins`, null, sources, 'contextPins'),
    sessionPath === null
      ? skip(sources, 'efficiency', 'GET', '/native/session/{id}/efficiency', 'No conversation is bound; efficiency effects are session-scoped.')
      : readJson(native, 'GET', `${sessionPath}/efficiency?limit=100`, null, sources, 'efficiency'),
    readJson(native, 'GET', '/native/semantic/capabilities', null, sources, 'semanticCapabilities'),
    readJson(native, 'GET', '/native/sessions/parallel', null, sources, 'parallelSessions'),
  ]);

  const [other, r8, parameters] = await Promise.all([otherPromise, r8Promise, parametersPromise]);

  const tasks = other[18];
  const taskId = firstTask(tasks) ?? firstRunTask(other[17]);
  const budgetPromise =
    sessionPath === null || taskId === null
      ? Promise.resolve(
          skip(
            sources,
            'budget',
            'GET',
            '/native/session/{id}/budget',
            'No task identity is served; the task budget is Unavailable.',
          ),
        )
      : readJson(
          native,
          'GET',
          `${sessionPath}/budget?taskId=${encodeURIComponent(taskId)}`,
          null,
          sources,
          'budget',
        );
  const proofPromise =
    sessionPath === null || taskId === null
      ? Promise.resolve(
          skip(
            sources,
            'proof',
            'GET',
            '/native/tasks/{id}/proof',
            'No completed task identity is served; engineering proof is Unavailable, never verified.',
          ),
        )
      : readJson(
          native,
          'GET',
          `/native/tasks/${encodeURIComponent(taskId)}/proof?session=${encodeURIComponent(session as string)}`,
          null,
          sources,
          'proof',
        );
  const [budget, proof] = await Promise.all([budgetPromise, proofPromise]);

  const sessionSearch =
    session === null
      ? skip(sources, 'search', 'GET', '/native/transcript/search', 'No conversation is bound; transcript search is session-scoped.')
      : skip(sources, 'search', 'GET', '/native/transcript/search', 'No search query has been issued; Find is user-initiated and the daemon refuses an empty needle.');
  const transcript = other[19];

  const criticalFailed = ['health', 'ready', 'models', 'config'].filter(
    (key) => sources[key]?.state !== 'ready',
  );
  const previous = options.previous;
  const stale = criticalFailed.length > 0;
  const staleAge = stale
    ? previous === null
      ? null
      : Math.max(0, now - previous.generated_at_ms)
    : null;

  const snapshot: E8Snapshot = {
    schema: 'faktor-edition8-ui/v1',
    revision: String(options.generation ?? previous?.revision ?? '0'),
    data_revision: dataRevision(sources, session, options),
    generated_at_ms: now,
    stale: stale && previous !== null,
    stale_age_ms: staleAge,
    binding: {
      session,
      workspace: options.workspace,
      worktree: options.worktree,
      daemon: options.daemon,
      generation: options.generation,
    },
    sources: sources as E8Sources,
    health,
    ready,
    models,
    capabilities,
    config,
    descriptors,
    connections,
    providers,
    parameters,
    presets: r8[0],
    instructions: r8[1],
    runtime: r8[2],
    provenance: r8[3],
    browserAdapters: r8[4],
    browserSessions: r8[5],
    contextPins: r8[6],
    efficiency: r8[7],
    semanticCapabilities: r8[8],
    parallelSessions: r8[9],
    budget,
    receipts: other[0],
    policy: other[1],
    permissionAudit: other[2],
    approvals: other[3],
    mcp: other[4],
    tools: other[5],
    skills: other[6],
    hooks: other[7],
    hookAudit: other[8],
    memory: other[9],
    sessions: other[10],
    sessionMeta: other[11],
    usage: other[12],
    sessionUsage: other[13],
    outcomes: other[14],
    proof,
    verification: other[15],
    checkpoints: other[16],
    taskRuns: other[17],
    tasks: other[18],
    turns: other[19],
    diagnostics: other[20],
    updater: other[21],
    identity: other[22],
    entitlements: unwrapEnvelope(other[23], 'entitlements'),
    browser: other[24],
    enterprise: other[25],
    workers: other[26],
    index: other[27],
    semantic: other[28],
    agents: other[29],
    board: other[30],
    tournaments: other[31],
    terminals: other[32],
    telemetry: other[33],
    search: sessionSearch,
    transcript,
  };
  // Ensure every declared source key has a typed row, even on an early error.
  for (const key of E8_SOURCE_KEYS) {
    if (snapshot.sources[key] === undefined) {
      (snapshot.sources as Record<string, E8Source>)[key] = emptySource(
        'GET',
        'unread',
        `${key} was not read in this pass.`,
      );
    }
  }
  return snapshot;
}

/** The empty pre-read snapshot. It claims nothing and names no data. */
export function emptyEdition8Snapshot(now = 0, binding: E8Snapshot['binding'] = null): E8Snapshot {
  const sources: Record<string, E8Source> = {};
  for (const key of E8_SOURCE_KEYS) {
    sources[key] = emptySource('GET', 'unread', `${key} has not been read yet.`);
  }
  return {
    schema: 'faktor-edition8-ui/v1',
    revision: '0',
    generated_at_ms: now,
    stale: true,
    stale_age_ms: null,
    binding,
    sources,
    health: null,
    ready: null,
    models: null,
    capabilities: null,
    config: null,
    descriptors: null,
    connections: null,
    providers: null,
    parameters: null,
    presets: null,
    instructions: null,
    runtime: null,
    provenance: null,
    browserAdapters: null,
    browserSessions: null,
    contextPins: null,
    efficiency: null,
    semanticCapabilities: null,
    parallelSessions: null,
    budget: null,
    receipts: null,
    policy: null,
    permissionAudit: null,
    approvals: null,
    mcp: null,
    tools: null,
    skills: null,
    hooks: null,
    hookAudit: null,
    memory: null,
    sessions: null,
    sessionMeta: null,
    search: null,
    transcript: null,
    usage: null,
    sessionUsage: null,
    outcomes: null,
    proof: null,
    verification: null,
    checkpoints: null,
    taskRuns: null,
    tasks: null,
    turns: null,
    diagnostics: null,
    updater: null,
    identity: null,
    entitlements: null,
    browser: null,
    enterprise: null,
    workers: null,
    index: null,
    semantic: null,
    agents: null,
    board: null,
    tournaments: null,
    terminals: null,
    telemetry: null,
  };
}
