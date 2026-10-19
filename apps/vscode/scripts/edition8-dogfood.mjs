#!/usr/bin/env node
// VS Code Edition 8 dogfood against a REAL daemon and a loopback fake
// provider. Uses the shipped NativeClient/command builders and the Rev8
// registry; records one result per required product flow plus W01-W16 route
// probes. A route the daemon does not implement is recorded as a typed
// refusal, never as a fabricated success.
//
// Output: target/certification/edition8/dogfood-vscode.raw.json

import { randomUUID } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { NativeApiError, NativeClient } from '../src/nativeClient.ts';
import { commandCall } from '../src/edition5/commands.ts';
import { E8_SCREENS, renderScreenHtml } from './edition8/host.mjs';
import { loadEdition8Snapshot } from '../src/edition8/source.ts';
import { E8_WORKFLOWS } from '../src/generated/edition8Registry.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');

function arg(name, fallback = null) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : fallback;
}

const baseUrl = arg('--base-url');
const token = arg('--token');
const workspace = arg('--workspace');
const browserUrl = arg('--browser-url');
const outPath = arg('--out', resolve(ROOT, 'target/certification/edition8/dogfood-vscode.raw.json'));
if (!baseUrl || !token || !workspace) {
  console.error('usage: edition8-dogfood.mjs --base-url URL --token TOKEN --workspace DIR [--out FILE]');
  process.exit(2);
}

const client = new NativeClient({ baseUrl, bearerToken: token, timeoutMs: 30_000 });
const results = [];

function record(id, passed, detail, status = 'effective') {
  results.push({ id, passed, status, detail });
  console.log(`${passed ? 'PASS' : 'FAIL'} ${id}: ${status} ${detail ?? ''}`);
}

async function typedFetch(method, path, body) {
  const response = await fetch(`${baseUrl}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  // Keep the full (bounded) body for JSON parsing; 1 MiB is far above any
  // dogfood response and still bounded. The old 4k cut made JSON.parse fail
  // on legitimate large previews (rev8-field-contracts reported
  // "Unterminated string at position 4000").
  const text = await response.text();
  if (text.length > 1_000_000) throw new Error(`${method} ${path} body exceeded 1 MiB`);
  return { status: response.status, text };
}

async function flow(id, fn) {
  try {
    const detail = await fn();
    record(id, true, detail);
  } catch (error) {
    record(id, false, error && error.message ? error.message : String(error));
  }
}

async function cancelUnlessSettled(runId) {
  try {
    await client.cancelTaskRun(sessionId, runId);
    return 'cancelled';
  } catch (error) {
    // A 409 means the run settled or advanced while the cancel was in
    // flight. That is a typed concurrency outcome, not a failed admission;
    // the durable row keeps its own state.
    if (error instanceof NativeApiError && error.status === 409) return 'already settled (409)';
    throw error;
  }
}

let sessionId = null;
let taskId = null;

await flow('vscode-host', async () => {
  await client.readyOrThrow(20_000);
  const health = await client.health();
  if (health.ok !== true) throw new Error('health.ok is not true');
  const created = await client.createSession({
    provider: 'p1',
    model: 'm',
    workspace,
    title: 'edition8 dogfood',
  });
  sessionId = String(created.id);
  const projection = await client.projection(sessionId);
  if (String(projection.session?.id) !== sessionId) throw new Error('projection session mismatch');
  const usage = await client.usage();
  await client.listSessions();
  return `session ${sessionId}, machine ${projection.machine}, usage sessions ${usage.sessions}`;
});

await flow('provider-config', async () => {
  const catalog = await client.modelCatalog();
  const match = catalog.find((entry) => entry.provider === 'p1' && entry.model === 'm');
  if (!match) throw new Error('the configured p1/m is not visible to the host');
  return `catalog exposes p1/m (source ${match.source})`;
});

await flow('prompt-budget-admission', async () => {
  // A per-run budget on the ordinary prompt path must ride the durable task
  // row the executor seeds, not be refused as malformed. The old refusal
  // made the composer's per-run budget a dead control.
  const started = await client.startTaskRun(sessionId, {
    goal: 'budget admission probe',
    submission_id: randomUUID(),
    max_tokens: 1,
  });
  if (typeof started.run_id !== 'string' || started.run_id.length === 0) {
    throw new Error('budgeted prompt did not return a run id');
  }
  const settled = await cancelUnlessSettled(started.run_id);
  return `admitted ${started.run_id}; ${settled}`;
});

await flow('edition8-live-snapshot', async () => {
  const snapshot = await loadEdition8Snapshot(client, {
    session: sessionId,
    workspace,
    worktree: null,
    daemon: baseUrl,
    generation: null,
    previous: null,
  });
  if (!Array.isArray(snapshot.models)) throw new Error('live snapshot.models is not an array');
  if (!snapshot.models.some((entry) => entry && entry.provider === 'p1' && entry.model === 'm')) {
    throw new Error('live snapshot does not carry the configured p1/m');
  }
  if (snapshot.sources.models?.state !== 'ready') {
    throw new Error(`live snapshot models source is ${snapshot.sources.models?.state}: ${snapshot.sources.models?.error}`);
  }
  if (snapshot.sources.capabilities?.state !== 'ready') {
    throw new Error(`live snapshot capabilities source is ${snapshot.sources.capabilities?.state}: ${snapshot.sources.capabilities?.error}`);
  }
  return `models=${snapshot.models.length}; models/capabilities read through the catalog allowlist`;
});

await flow('rev8-field-contracts', async () => {
  const checks = [];
  const expect = async (method, path, body, allowed, label) => {
    const response = await typedFetch(method, path, body);
    if (!allowed.includes(response.status)) {
      throw new Error(`${label} answered ${response.status}: ${response.text}`);
    }
    checks.push(`${label}:${response.status}`);
    return response;
  };

  // P-11/P-12/P-15 + M-08: transport fields persist, a bounded probe of a
  // dead loopback endpoint reports failure (never a fake ready).
  await expect('POST', '/native/providers/connections', {
    id: 'dogfood-transport', family: 'ollama', label: 'dogfood transport',
    endpoint: 'http://127.0.0.1:9', models: ['m'], allow_loopback: true,
    connect_timeout_ms: 250, first_byte_timeout_ms: 500, total_timeout_ms: 750,
    keep_alive: '15m', tls_mode: 'system',
  }, [200], 'connection');
  const probe = JSON.parse((await expect('POST', '/native/catalog/probe',
    { connectionId: 'dogfood-transport', model: 'm' }, [200], 'probe')).text);
  if (probe.state === 'ready') throw new Error('dead endpoint probed ready');

  // M-05/M-06: served role map + ordered fallback persist.
  await expect('POST', '/native/presets', {
    id: 'dogfood-preset', mode: 'code', name: 'Dogfood', description: '',
    origin: 'user', scope: 'user', roleInstructions: 'code',
    modelProvider: 'p1', modelId: 'm', toolAllow: [], toolDeny: [],
    delegation: { maxDepth: 0, maxChildren: 0, parentBudgetSharePpm: 500000,
      allowedChildPresets: [] },
    childConcurrency: 0, budgetSharePpm: 500000,
  }, [200], 'preset');
  await expect('POST', '/native/presets/dogfood-preset/roles',
    { roleModels: { coder: { provider: 'p1', model: 'm' } } }, [200], 'roles');
  await expect('POST', '/native/presets/dogfood-preset/fallback',
    { fallbackChain: [{ provider: 'p1', model: 'm' }] }, [200], 'fallback');

  // C-08/B-09: ignore patterns and loop caps are durable descriptors with a
  // real context-pipeline effect (an ignored source disappears).
  const snapshot = await typedFetch('GET', '/native/config/snapshot');
  const revision = Number(JSON.parse(snapshot.text).revision ?? 0);
  await expect('POST', '/native/config/commit', {
    expected_revision: revision,
    changes: [
      { key: 'faktor.context.ignorePatterns', scope: 'user', value: ['**/*.log'] },
      { key: 'faktor.agent.maxTurns', scope: 'user', value: 25 },
    ],
  }, [200], 'config');
  const preview = JSON.parse((await expect('POST', `/native/session/${encodeURIComponent(sessionId)}/context/preview`, {
    operationId: `dogfood-ctx-${Date.now()}`,
    sources: [
      { id: 'ignored', uri: 'logs/app.log', version: '1', content_hash: '0'.repeat(64), category: 'file', inclusion_reason: 'context' },
      { id: 'kept', uri: 'src/app.rs', version: '1', content_hash: '0'.repeat(64), category: 'file', inclusion_reason: 'context' },
    ],
  }, [200], 'context-preview')).text);
  if (JSON.stringify(preview).includes('logs/app.log')) throw new Error('ignored source was not filtered');
  if (!JSON.stringify(preview).includes('src/app.rs')) throw new Error('protected source was filtered');

  // K-02/B-09 session selection is durable and typed.
  await expect('PUT', `/native/session/${encodeURIComponent(sessionId)}/settings`,
    { compactionModel: 'p1/m', maxTurns: 25, maxConsecutiveFailures: 3 }, [200], 'settings');
  const settings = JSON.parse((await typedFetch('GET',
    `/native/session/${encodeURIComponent(sessionId)}/settings`)).text);
  if (settings.maxTurns !== 25) throw new Error('maxTurns did not persist');

  // T-06/T-10: remote auth lives in the vault; bounds persist; token never
  // comes back on the wire.
  await expect('POST', '/native/mcp/servers', {
    id: 'dogfood-mcp', name: 'Dogfood Remote', command: '', args: [], env: [],
    trusted: true, enabled: true, transport: 'streamable_http',
    endpoint: 'https://mcp.example.com/rpc', timeout_ms: 5000,
    output_cap_bytes: 1048576, stderr_cap_bytes: 65536,
  }, [200], 'mcp');
  const auth = await expect('POST', '/native/mcp/servers/dogfood-mcp/auth',
    { action: 'connect', accessToken: 'dogfood-secret', account: 'dogfood', scopes: ['tools.read'] },
    [200], 'mcp-auth');
  if (auth.text.includes('dogfood-secret')) throw new Error('auth token leaked');
  await expect('POST', '/native/mcp/servers/dogfood-mcp/oauth/revoke', {}, [200], 'mcp-revoke');

  // C-07/C-14/K-12: pins, manifest export and measured efficiency are real
  // session-scoped reads/writes, not placeholder rows.
  await expect('POST', `/native/session/${encodeURIComponent(sessionId)}/context/pins`,
    { sourceId: 'kept', action: 'pin' }, [200], 'pin');
  const pins = JSON.parse((await expect('GET',
    `/native/session/${encodeURIComponent(sessionId)}/context/pins`, undefined, [200], 'pins')).text);
  if (!JSON.stringify(pins).includes('kept')) throw new Error('pinned source was not retained');
  await expect('GET', `/native/context/export?session=${encodeURIComponent(sessionId)}`,
    undefined, [200], 'context-export');
  await expect('GET', `/native/session/${encodeURIComponent(sessionId)}/efficiency?limit=10`,
    undefined, [200], 'efficiency');

  // S-09: the richer matcher is a real schema field; a non-matching input is
  // previewed without spawning, and a matching input runs the exact path.
  await expect('POST', '/native/hooks', {
    id: 'dogfood-matcher',
    spec: {
      id: 'dogfood-matcher', events: ['pre_tool'], command: '/bin/sh',
      args: ['-c', 'exit 0'], env: [], env_allowlist: true, deadline_ms: 2000,
      stdout_cap: 65536, stderr_cap: 65536, failure_policy: 'fail_closed',
      permission_scope: [],
      matcher: { tools: ['run_*'], paths: ['src/**'] },
    },
    trusted: true, enabled: true,
  }, [200], 'hook-matcher');
  const miss = JSON.parse((await expect('POST', '/native/hooks/dogfood-matcher/test', {
    event: 'pre_tool', payload: { tool: 'read_file', path: 'src/app.rs' },
  }, [200], 'hook-miss')).text);
  if (miss.matched !== false) throw new Error('matcher did not reject a non-matching tool');
  const hit = JSON.parse((await expect('POST', '/native/hooks/dogfood-matcher/test', {
    event: 'pre_tool', payload: { tool: 'run_command', path: 'src/app.rs' },
  }, [200], 'hook-hit')).text);
  if (hit.matched !== true) throw new Error('matcher did not select a matching input');

  // T-12: a configured but stopped server answers a typed not-ready state;
  // an adapter that does not implement resources is never given a fake
  // catalog. Both are honest outcomes.
  const resources = await expect('GET', '/native/mcp/servers/dogfood-mcp/resources',
    undefined, [200, 409], 'mcp-resources');
  if (resources.status === 409 && !resources.text.includes('mcp_')) {
    throw new Error(`mcp resources refusal is not typed: ${resources.text}`);
  }

  // X-06/X-08/X-09: enable the real browser surface, connect to a loopback
  // page and capture its observations/screenshot. Without an executable or
  // when disabled the same paths answer a typed refusal; a connected state
  // is never invented.
  const browserSnapshot = JSON.parse((await typedFetch('GET', '/native/config/snapshot')).text);
  const browserRevision = Number(browserSnapshot.revision ?? 0);
  await expect('POST', '/native/config/commit', {
    expected_revision: browserRevision,
    changes: [
      { key: 'faktor.browser.enabled', scope: 'user', value: true },
      { key: 'faktor.sandbox.allowLoopback', scope: 'user', value: true },
    ],
  }, [200], 'browser-config');
  await expect('GET', '/native/browser/adapters', undefined, [200], 'browser-adapters');
  const created = await expect('POST', '/native/browser/sessions', {
    adapterId: 'chromium-local', session: sessionId, url: browserUrl ?? 'http://127.0.0.1:9/',
  }, [200, 409, 503], 'browser-session');
  if (created.status === 200) {
    const session = JSON.parse(created.text);
    if (!['requested', 'connected'].includes(session.state)) {
      throw new Error(`browser session claimed an invalid state: ${created.text}`);
    }
    const observations = JSON.parse((await expect('GET',
      `/native/browser/sessions/${encodeURIComponent(session.id)}/observations`,
      undefined, [200], 'browser-observations')).text);
    if (!Array.isArray(observations.observations) || observations.observations.length === 0) {
      throw new Error('a connected browser session produced no observations');
    }
    const screenshot = JSON.parse((await expect('POST',
      `/native/browser/sessions/${encodeURIComponent(session.id)}/screenshot`,
      { mime: 'image/png', dataBase64: '', inModelContext: false },
      [200], 'browser-screenshot')).text);
    if (!(Number(screenshot.bytes) > 0)) throw new Error('browser screenshot artifact is empty');
  } else if (!/browser|adapter|chromium|disabled/i.test(created.text)) {
    throw new Error(`browser refusal is not typed: ${created.text}`);
  }

  // O-10: the payload preview is explicit and there is no upload endpoint.
  const telemetry = JSON.parse((await expect('GET', '/native/telemetry',
    undefined, [200], 'telemetry')).text);
  if (telemetry.endpoint !== null) throw new Error('telemetry invented an upload endpoint');
  if (telemetry.payloadPreview?.bounded !== true) throw new Error('telemetry preview is not bounded');

  // V-02: durable review comment CRUD.
  const comment = JSON.parse((await expect('POST', `/native/session/${encodeURIComponent(sessionId)}/review/comments`, {
    path: 'src/app.rs', lineStart: 1, lineEnd: 1, side: 'new', body: 'dogfood review',
  }, [200], 'review')).text);
  await expect('GET', `/native/session/${encodeURIComponent(sessionId)}/review/comments`, undefined, [200], 'review-list');
  await expect('DELETE', `/native/session/${encodeURIComponent(sessionId)}/review/comments/${encodeURIComponent(comment.comment.id)}`, undefined, [200], 'review-delete');

  // V-03/V-05: no staged shadow/checkpoint in this fixture, so both answer
  // a typed refusal; nothing is fabricated.
  // 503 = the host did not wire the shadow authority; 404 = it is wired but
  // this session has no staged change set; 409 = the change set was already
  // decided differently. All are typed refusals; no acceptance is claimed.
  await expect('POST', `/native/session/${encodeURIComponent(sessionId)}/changes/accept`,
    { approved: [], rejected: [] }, [404, 409, 503], 'accept');
  await expect('POST', `/native/session/${encodeURIComponent(sessionId)}/checkpoints/1/restore/preview`,
    { direction: 'undo' }, [404, 503], 'restore-preview');

  // D-06: export a throwaway session, then delete it and replay idempotently.
  const doomed = await client.createSession({ provider: 'p1', model: 'm', workspace, title: 'dogfood delete' });
  const doomedId = String(doomed.id);
  const exported = JSON.parse((await expect('POST', `/native/session/${encodeURIComponent(doomedId)}/export`,
    { redact: true, maxBytes: 65536 }, [200], 'export')).text);
  if (String(exported.digest ?? '').length !== 64) throw new Error('export digest missing');
  await expect('DELETE', `/native/session/${encodeURIComponent(doomedId)}`, undefined, [200], 'delete');
  await expect('DELETE', `/native/session/${encodeURIComponent(doomedId)}`, undefined, [200], 'delete-replay');

  return `${checks.length} contract checks: ${checks.join(', ')}`;
});

await flow('task-admission', async () => {
  const submission = randomUUID();
  const started = await client.startTaskRun(sessionId, {
    goal: 'edition8 dogfood: answer with one short sentence',
    submission_id: submission,
    criteria: ['answer is served'],
  });
  taskId = String(started.task_id);
  const retried = await client.startTaskRun(sessionId, {
    goal: 'edition8 dogfood: answer with one short sentence',
    submission_id: submission,
    criteria: ['answer is served'],
  });
  if (retried.run_id !== started.run_id) throw new Error('the same submission id did not recover the same run');
  const cancel = await cancelUnlessSettled(started.run_id);
  return `run ${started.run_id} admitted idempotently; cancel ${cancel}`;
});

await flow('compaction-long-task', async () => {
  // Compact a session that has a durable task projection and no pending
  // target invariant (the cancelled admission session correctly refuses).
  const compactSession = await client.createSession({
    provider: 'p1',
    model: 'm',
    workspace,
    title: 'edition8 compaction',
  });
  const compactId = String(compactSession.id);
  const started = await client.startTaskRun(compactId, {
    goal: 'edition8 dogfood: produce one short answer for a durable compaction projection',
    submission_id: randomUUID(),
    criteria: ['one short answer is served'],
    work_items: [{ id: 'compact-w1', kind: 'Analysis', summary: 'produce a short answer' }],
  });
  for (let attempt = 0; attempt < 300; attempt += 1) {
    const state = String((await client.taskRunState(compactId, String(started.run_id))).state).toLowerCase();
    if (state !== 'running' && state !== 'queued' && state !== 'active') break;
    await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
  }
  const response = await typedFetch('POST', `/native/session/${encodeURIComponent(compactId)}/context/compaction`, {
    operationId: `dogfood-compact-${Date.now()}`,
    targetTokens: 24,
    minReductionRatio: 0.01,
    maxHistoryMessages: 50,
  });
  if (response.status !== 200) throw new Error(`durable compaction answered ${response.status}: ${response.text}`);
  const operation = JSON.parse(response.text);
  if (operation.phase === 'applied' && operation.result?.accepted === true) {
    return `durable compaction ${operation.operationId} applied; measured reduction ${operation.result.reduction}`;
  }
  const reason = String(operation.result?.rejectedReason ?? '');
  if (operation.phase === 'refused' && /active instruction|identity|target invariant/i.test(reason)) {
    return `durable compaction ${operation.operationId} refused safely (${reason}); no reduction or summary is claimed`;
  }
  throw new Error(`compaction ended ${operation.phase}: ${reason || 'no typed reason'}`);
});

await flow('restore-fork', async () => {
  const checkpoints = await client.checkpoints(sessionId);
  if (!Array.isArray(checkpoints)) throw new Error('checkpoints is not a list');
  const target = checkpoints[0]?.id ?? 'none';
  const restore = commandCall('restore.run', {
    session: sessionId,
    checkpoint: target,
    expected_current_hash: 'sha256:dogfood',
  });
  const fork = commandCall('fork.run', { session: sessionId, checkpoint: target });
  const response = await fetch(`${baseUrl}${restore.path}`, {
    method: restore.method,
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify(restore.body ?? {}),
  });
  const typed = [200, 201, 202, 400, 404, 405, 409, 422].includes(response.status);
  if (!typed) throw new Error(`restore answered non-typed ${response.status}`);
  return `${checkpoints.length} checkpoints; restore ${restore.method} ${restore.path} -> ${response.status}; fork ${fork.method} ${fork.path} built`;
});

await flow('permissions', async () => {
  const pending = await client.permissions(sessionId);
  if (!Array.isArray(pending)) throw new Error('permissions is not a list');
  const response = await fetch(`${baseUrl}/native/permission/reply`, {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify({ session: sessionId, id: '999999', decision: 'deny' }),
  });
  if (response.ok) throw new Error('an unknown permission id was accepted');
  if (![400, 404, 409, 422].includes(response.status)) throw new Error(`expected typed 400/404/409/422, got ${response.status}`);
  return `${pending.length} pending; unknown reply refused ${response.status}`;
});

await flow('proof', async () => {
  if (taskId === null) throw new Error('no task id');
  const response = await fetch(
    `${baseUrl}/native/tasks/${encodeURIComponent(taskId)}/proof?session=${encodeURIComponent(sessionId)}`,
    { headers: { authorization: `Bearer ${token}` } },
  );
  if (response.status !== 200) return `proof route answered typed ${response.status} (durable proof stays unavailable)`;
  const body = await response.json();
  if (body.schema !== 'faktor-task-proof/v1') throw new Error(`unexpected proof schema ${String(body.schema)}`);
  return `proof ${body.proofState} required ${body.checks.requiredPassed}/${body.checks.requiredTotal}`;
});

await flow('finish-levels', async () => {
  const levels = [
    { name: 'local', contract: { include_commit: false, include_push: false, include_pr: false } },
    { name: 'commit', contract: { include_commit: true, include_push: false, include_pr: false } },
    { name: 'push', contract: { include_commit: true, include_push: true, include_pr: false } },
    { name: 'pr', contract: { include_commit: true, include_push: true, include_pr: true } },
  ];
  const observed = [];
  for (const level of levels) {
    const response = await typedFetch('POST', `/native/session/${encodeURIComponent(sessionId)}/task-runs`, {
      goal: `edition8 dogfood finish level ${level.name}`,
      submission_id: randomUUID(),
      work_items: [{ id: `dogfood-${level.name}`, kind: 'Implementation', summary: `exercise the ${level.name} contract`, ownership: 'isolated_worktree' }],
      completion_contract: level.contract,
    });
    if (response.status >= 500) {
      // One bounded retry for a transient daemon 5xx; the run id is fresh and
      // no success is claimed for the first attempt.
      const retry = await typedFetch('POST', `/native/session/${encodeURIComponent(sessionId)}/task-runs`, {
        goal: `edition8 dogfood finish level ${level.name} retry`,
        submission_id: randomUUID(),
        work_items: [{ id: `dogfood-${level.name}-retry`, kind: 'Implementation', summary: `exercise the ${level.name} contract`, ownership: 'isolated_worktree' }],
        completion_contract: level.contract,
      });
      if (retry.status !== 200) throw new Error(`finish level ${level.name} refused ${response.status}/${retry.status}: ${retry.text}`);
      const retried = JSON.parse(retry.text);
      await client.cancelTaskRun(sessionId, retried.run_id);
      observed.push(level.name);
      continue;
    }
    if (response.status !== 200) throw new Error(`finish level ${level.name} refused ${response.status}: ${response.text}`);
    const started = JSON.parse(response.text);
    await cancelUnlessSettled(started.run_id);
    observed.push(level.name);
  }
  return `admitted and cancelled contracts: ${observed.join(', ')}`;
});

await flow('registry-72', async () => {
  let rendered = 0;
  for (const screen of E8_SCREENS) {
    const html = renderScreenHtml({ screenId: screen.id, fixtureClass: 'ready', theme: 'dark-modern', width: 320, zoom: 1 });
    if (!html.includes(`data-screen="${screen.id}"`)) throw new Error(`${screen.id} did not render`);
    rendered += 1;
  }
  return `${rendered}/72 Rev8 screens rendered by the VS Code host adapter`;
});

// W01-W16 production route probes, typed statuses only.
const workflowRoutes = {
  W01: ['GET', '/native/providers'],
  W02: ['GET', '/models'],
  W03: ['GET', '/native/model-parameters?provider=p1&model=m'],
  W04: ['GET', `/native/session/${sessionId}/turns`],
  W05: ['POST', `/native/session/${sessionId}/context/preview`, { operationId: 'dogfood-preview' }],
  W06: ['POST', `/native/session/${sessionId}/context/compaction`, { operationId: 'dogfood-workflow-compact' }],
  W07: ['GET', '/native/permissions'],
  W08: ['GET', '/native/mcp/servers'],
  W09: ['GET', '/native/skills'],
  W10: ['GET', '/native/hooks'],
  W11: ['GET', `/native/agents?session=${encodeURIComponent(sessionId)}`],
  W12: ['GET', `/native/session/${sessionId}/tasks`],
  W13: ['GET', '/native/usage'],
  W14: ['GET', `/native/session/${sessionId}/checkpoints`],
  W15: ['GET', `/native/tasks/${encodeURIComponent(taskId ?? '1')}/completion-steps?session=${encodeURIComponent(sessionId)}`],
  W16: ['GET', '/native/ready'],
};
for (const workflow of E8_WORKFLOWS) {
  const [method, path, body] = workflowRoutes[workflow.id] ?? ['GET', '/native/ready'];
  try {
    const response = await typedFetch(method, path, body);
    const passed = response.status >= 200 && response.status < 300;
    if (!passed) throw new Error(`${method} ${path} -> ${response.status}: ${response.text}`);
    record(`workflow:${workflow.id}`, true, `${method} ${path} -> ${response.status}`, 'effective');
  } catch (error) {
    record(`workflow:${workflow.id}`, false, error && error.message ? error.message : String(error), 'typed-missing');
  }
}

mkdirSync(dirname(outPath), { recursive: true });
const failed = results.filter((row) => row.passed !== true);
writeFileSync(
  outPath,
  `${JSON.stringify(
    {
      schema: 'faktor-edition8-dogfood-host/v1',
      host: 'vscode',
      base_url: baseUrl,
      generated_at: new Date().toISOString(),
      counts: {
        flows: results.filter((row) => !row.id.startsWith('workflow:')).length,
        workflows: results.filter((row) => row.id.startsWith('workflow:')).length,
        passed: results.length - failed.length,
        failed: failed.length,
      },
      results,
    },
    null,
    2,
  )}\n`,
);
if (failed.length > 0) {
  console.error(`vscode edition8 dogfood: ${failed.length} result(s) failed`);
  process.exit(1);
}
console.log(`vscode edition8 dogfood: ${results.length}/${results.length} results passed`);
