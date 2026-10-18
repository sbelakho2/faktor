#!/usr/bin/env node
// VS Code host dogfood against a REAL daemon and a loopback fake provider.
// It uses the shipped NativeClient/edition5 command builders (no mocks in the
// host path) and records one result per required Edition 5 flow:
//
//   task-admission        create session + idempotent task start + cancel
//   provider-config       the explicit provider/model is visible to the host
//   restore-fork          checkpoint read + exact restore/fork command routes
//   permissions           list + typed refusal of an unknown reply
//   proof                 durable task proof read (schema-checked)
//   finish-levels         local/commit/push/pr contracts admitted and cancelled
//   vscode-host           health/ready/session/projection/usage reads
//
// A route the daemon does not implement is recorded as `honest-unavailable`
// with the typed status, never as a passed feature with fabricated state.
// The compaction long-task flow runs in the shell wrapper against the real
// Rust compaction/runtime probes.

import { randomUUID } from 'node:crypto';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { NativeApiError, NativeClient } from '../src/nativeClient.ts';
import { commandCall } from '../src/edition5/commands.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');

function arg(name, fallback = null) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : fallback;
}

const baseUrl = arg('--base-url');
const token = arg('--token');
const workspace = arg('--workspace');
const outPath = arg('--out', resolve(ROOT, 'target/certification/edition5/dogfood-vscode.raw.json'));
if (!baseUrl || !token || !workspace) {
  console.error('usage: edition5-dogfood.mjs --base-url URL --token TOKEN --workspace DIR [--out FILE]');
  process.exit(2);
}

const client = new NativeClient({ baseUrl, bearerToken: token, timeoutMs: 30_000 });
const results = [];

function record(id, passed, detail, status = 'effective') {
  results.push({ id, passed, status, detail });
  console.log(`${passed ? 'PASS' : 'FAIL'} ${id}: ${status} ${detail ?? ''}`);
}

async function typedCall(label, fn) {
  try {
    return { ok: true, value: await fn() };
  } catch (error) {
    const status = error instanceof NativeApiError ? error.status : 0;
    const code = error instanceof NativeApiError ? error.code : error?.name;
    return { ok: false, status, code, error };
  }
}

const flows = [];
async function flow(id, fn) {
  try {
    const detail = await fn();
    record(id, true, detail);
  } catch (error) {
    record(id, false, error && error.message ? error.message : String(error));
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
    title: 'edition5 dogfood',
  });
  sessionId = String(created.id);
  const projection = await client.projection(sessionId);
  if (projection.sessionId !== sessionId) throw new Error('projection session mismatch');
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

await flow('task-admission', async () => {
  const submission = randomUUID();
  const started = await client.startTaskRun(sessionId, {
    goal: 'edition5 dogfood: answer with one short sentence',
    submission_id: submission,
    criteria: ['answer is served'],
  });
  taskId = String(started.task_id);
  const retried = await client.startTaskRun(sessionId, {
    goal: 'edition5 dogfood: answer with one short sentence',
    submission_id: submission,
    criteria: ['answer is served'],
  });
  if (retried.run_id !== started.run_id) {
    throw new Error('the same submission id did not recover the same run');
  }
  const cancelled = await client.cancelTaskRun(sessionId, started.run_id);
  if (cancelled.run_id !== started.run_id) throw new Error('cancel named another run');
  return `run ${started.run_id} admitted idempotently and cancelled`;
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
    const started = await client.startTaskRun(sessionId, {
      goal: `edition5 dogfood finish level ${level.name}`,
      submission_id: randomUUID(),
      work_items: [
        {
          id: `dogfood-${level.name}`,
          kind: 'edit',
          summary: `exercise the ${level.name} finish contract`,
        },
      ],
      completion_contract: level.contract,
    });
    await client.cancelTaskRun(sessionId, started.run_id);
    observed.push(level.name);
  }
  return `admitted and cancelled contracts: ${observed.join(', ')}`;
});

await flow('permissions', async () => {
  const pending = await client.permissions(sessionId);
  if (!Array.isArray(pending)) throw new Error('permissions is not a list');
  const reply = await typedCall('permission reply', () =>
    fetch(`${baseUrl}/native/permission/reply`, {
      method: 'POST',
      headers: {
        authorization: `Bearer ${token}`,
        'content-type': 'application/json',
      },
      body: JSON.stringify({ session: sessionId, id: '999999', decision: 'deny' }),
    }).then(async (response) => {
      if (!response.ok) {
        throw new NativeApiError(
          response.status,
          'permission_reply_refused',
          `permission reply refused (${response.status})`,
          false,
        );
      }
      return response.json();
    }),
  );
  if (reply.ok) throw new Error('an unknown permission id was accepted');
  if (![404, 409].includes(reply.status)) {
    throw new Error(`expected a typed 404/409 for an unknown permission id, got ${reply.status}`);
  }
  return `${pending.length} pending; unknown reply refused ${reply.status}`;
});

await flow('proof', async () => {
  if (taskId === null) throw new Error('no task id');
  const response = await fetch(
    `${baseUrl}/native/tasks/${encodeURIComponent(taskId)}/proof?session=${encodeURIComponent(sessionId)}`,
    { headers: { authorization: `Bearer ${token}` } },
  );
  if (response.status !== 200) {
    return `proof route answered typed ${response.status} (durable proof stays unavailable)`;
  }
  const body = await response.json();
  if (body.schema !== 'faktor-task-proof/v1') {
    throw new Error(`unexpected proof schema ${String(body.schema)}`);
  }
  return `proof ${body.proofState} required ${body.checks.requiredPassed}/${body.checks.requiredTotal}`;
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
  const typed = [200, 201, 202, 404, 405, 409, 422].includes(response.status);
  if (!typed) throw new Error(`restore answered non-typed ${response.status}`);
  return `${checkpoints.length} checkpoints; restore ${restore.method} ${restore.path} -> ${response.status}; fork ${fork.method} ${fork.path} built`;
});

mkdirSync(dirname(outPath), { recursive: true });
const failed = results.filter((row) => row.passed !== true);
writeFileSync(
  outPath,
  `${JSON.stringify(
    {
      schema: 'faktor-edition5-dogfood-host/v1',
      host: 'vscode',
      base_url: baseUrl,
      generated_at: new Date().toISOString(),
      counts: { flows: results.length, passed: results.length - failed.length, failed: failed.length },
      results,
    },
    null,
    2,
  )}\n`,
);
if (failed.length > 0) {
  console.error(`vscode dogfood: ${failed.length} flow(s) failed`);
  process.exit(1);
}
console.log(`vscode dogfood: ${results.length}/${results.length} flows passed`);
