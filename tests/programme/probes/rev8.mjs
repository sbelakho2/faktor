#!/usr/bin/env node
// Revision-8 route contract probe: the 17 routes the report found without a
// registry row, plus the four credential-vault routes, exercised over HTTP
// against the real daemon. Every assertion is route-specific (round trip,
// typed refusal, stale revision, tombstone, secret non-disclosure), not a
// shared authentication invariant.

import { execFileSync, spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { CREDENTIAL_ROUTES, REV8_ROUTE_IDS } from '../discovery.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT_DIR = resolve(ROOT, 'target/certification/programme');

function sha(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

function binary() {
  const candidates = [
    process.env.FAKTOR_CLI_BIN,
    resolve(ROOT, 'target/release/faktor-cli'),
    resolve(ROOT, 'target/debug/faktor-cli'),
  ].filter(Boolean);
  return candidates.find((candidate) => existsSync(candidate)) ?? null;
}

async function start(binaryPath, config, data, token) {
  const child = spawn(binaryPath, ['serve', '--port', '0', '--data-dir', data, '--config', config], {
    cwd: ROOT,
    env: { ...process.env, FAKTOR_SERVER_PASSWORD: token, REV8_PROBE_KEY: 'loopback-only' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  child.stdout.on('data', (chunk) => {
    output += chunk.toString();
  });
  child.stderr.on('data', (chunk) => {
    output += chunk.toString();
  });
  const deadline = Date.now() + 25_000;
  let base = null;
  while (Date.now() < deadline) {
    const match = /(http:\/\/127\.0\.0\.1:\d+)/.exec(output);
    if (match) {
      base = match[1];
      break;
    }
    if (child.exitCode !== null) break;
    await new Promise((done) => setTimeout(done, 40));
  }
  if (base === null) {
    child.kill('SIGKILL');
    throw new Error(`rev8 probe daemon did not start: ${output.slice(-1200)}`);
  }
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${base}/native/ready`, {
        headers: { authorization: `Bearer ${token}` },
      });
      if (response.ok) return { child, base };
    } catch {
      // poll loopback
    }
    await new Promise((done) => setTimeout(done, 40));
  }
  child.kill('SIGKILL');
  throw new Error(`rev8 probe daemon never became ready: ${output.slice(-1200)}`);
}

async function call(base, token, method, path, body) {
  const response = await fetch(`${base}${path}`, {
    method,
    headers: {
      authorization: `Bearer ${token}`,
      ...(body === undefined ? {} : { 'content-type': 'application/json' }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = text.length > 0 ? JSON.parse(text) : null;
  } catch {
    json = null;
  }
  return { status: response.status, text, json };
}

function assertCase(results, route, name, condition, observed) {
  const entry = results.get(route) ?? { route, cases: [], passed: true };
  entry.cases.push({ name, passed: Boolean(condition), observed });
  if (!condition) entry.passed = false;
  results.set(route, entry);
}

export async function probeRev8({ report = false } = {}) {
  const binaryPath = binary();
  if (binaryPath === null) {
    return { status: 'deferred', reason: 'no faktor-cli binary; trusted lane builds it', results: [] };
  }
  const tmp = mkdtempSync(join(tmpdir(), 'faktor-rev8-probe-'));
  const data = join(tmp, 'data');
  const workspace = join(tmp, 'workspace');
  const config = join(tmp, 'faktor.json');
  mkdirSync(data, { recursive: true });
  mkdirSync(workspace, { recursive: true });
  writeFileSync(
    config,
    `${JSON.stringify({
      config_version: 1,
      model: 'm',
      sandbox: { network: [] },
      providers: [
        {
          kind: 'open_ai',
          id: 'p1',
          base_url: 'http://127.0.0.1:9/v1',
          api_key_env: 'REV8_PROBE_KEY',
          api: 'chat',
          allow_loopback: true,
          models: ['m'],
        },
      ],
    })}\n`,
  );
  const token = randomBytes(32).toString('hex');
  const results = new Map();
  let daemon = null;
  try {
    daemon = await start(binaryPath, config, data, token);
    const { base } = daemon;
    const sessionRes = await call(base, token, 'POST', '/native/session', {
      provider: 'p1',
      model: 'm',
      workspace,
      title: 'rev8 route probe',
    });
    if (sessionRes.status !== 200) {
      throw new Error(`rev8 probe session: ${sessionRes.status} ${sessionRes.text.slice(0, 300)}`);
    }
    const session = String(sessionRes.json.id);

    // ---------------------------------------------------- credential vault
    const secret = `sk-planted-${randomBytes(12).toString('hex')}`;
    const stored = await call(base, token, 'POST', '/native/providers/credentials', {
      kind: 'provider_key',
      label: 'probe',
      secret,
    });
    const handle = stored.json?.handle ?? 'vault://absent';
    assertCase(
      results,
      'native.post.native.providers.credentials',
      'stores without echoing the secret',
      stored.status === 200 && typeof handle === 'string' && !stored.text.includes(secret),
      { status: stored.status, handle },
    );
    const malformedStore = await call(base, token, 'POST', '/native/providers/credentials', {
      kind: 'provider_key',
    });
    assertCase(
      results,
      'native.post.native.providers.credentials',
      'malformed body is typed 400',
      malformedStore.status === 400,
      { status: malformedStore.status },
    );
    const listed = await call(base, token, 'GET', '/native/providers/credentials');
    assertCase(
      results,
      'native.get.native.providers.credentials',
      'lists references only',
      listed.status === 200 &&
        Array.isArray(listed.json?.credentials) &&
        listed.json.credentials.some((entry) => entry.handle === handle) &&
        !listed.text.includes(secret),
      { status: listed.status, count: listed.json?.credentials?.length },
    );
    const rotatedSecret = `sk-rotated-${randomBytes(12).toString('hex')}`;
    const rotated = await call(base, token, 'POST', '/native/providers/credentials/rotate', {
      handle,
      secret: rotatedSecret,
    });
    assertCase(
      results,
      'native.post.native.providers.credentials.rotate',
      'rotates a generation without disclosing either secret',
      rotated.status === 200 &&
        rotated.json?.handle === handle &&
        rotated.json?.generation >= 2 &&
        !rotated.text.includes(secret) &&
        !rotated.text.includes(rotatedSecret),
      { status: rotated.status, generation: rotated.json?.generation },
    );
    const deleted = await call(
      base,
      token,
      'DELETE',
      `/native/providers/credentials/${encodeURIComponent(handle.replace('vault://', ''))}`,
    );
    assertCase(
      results,
      'native.delete.native.providers.credentials.handle',
      'deletes locally and reports remoteRevoked=false',
      deleted.status === 200 && deleted.json?.remoteRevoked === false,
      { status: deleted.status },
    );

    // -------------------------------------------------- preset roles/fallback
    const preset = await call(base, token, 'POST', '/native/presets', {
      id: 'probe',
      mode: 'code',
      name: 'Probe',
      description: '',
      origin: 'user',
      scope: 'user',
      roleInstructions: 'code',
      modelProvider: 'p1',
      modelId: 'm',
      toolAllow: [],
      toolDeny: [],
      delegation: {
        maxDepth: 0,
        maxChildren: 0,
        parentBudgetSharePpm: 500000,
        allowedChildPresets: [],
      },
      childConcurrency: 0,
      budgetSharePpm: 500000,
    });
    const roles = await call(base, token, 'POST', '/native/presets/probe/roles', {
      roleModels: { coder: { provider: 'p1', model: 'm' } },
    });
    assertCase(
      results,
      'native.post.native.presets.id.roles',
      'round-trips a served role map',
      preset.status === 200 && roles.status === 200 && roles.json?.roleModels?.coder?.model === 'm',
      { preset: preset.status, status: roles.status },
    );
    const rolesRead = await call(base, token, 'GET', '/native/presets/probe/roles');
    assertCase(
      results,
      'native.get.native.presets.id.roles',
      'reports served=true for the served binding',
      rolesRead.status === 200 && rolesRead.json?.served?.coder?.served === true,
      { status: rolesRead.status },
    );
    const duplicateFallback = await call(base, token, 'POST', '/native/presets/probe/fallback', {
      fallbackChain: [
        { provider: 'p1', model: 'm', reason: 'first' },
        { provider: 'p1', model: 'm', reason: 'dup' },
      ],
    });
    assertCase(
      results,
      'native.post.native.presets.id.fallback',
      'refuses a duplicate chain before persistence',
      duplicateFallback.status === 400,
      { status: duplicateFallback.status },
    );
    const fallback = await call(base, token, 'POST', '/native/presets/probe/fallback', {
      fallbackChain: [{ provider: 'p1', model: 'm', reason: 'only' }],
    });
    const fallbackRead = await call(base, token, 'GET', '/native/presets/probe/fallback');
    assertCase(
      results,
      'native.get.native.presets.id.fallback',
      'returns the ordered chain with served status',
      fallback.status === 200 &&
        fallbackRead.status === 200 &&
        fallbackRead.json?.candidates?.[0]?.served === true,
      { write: fallback.status, read: fallbackRead.status },
    );
    const unknownRoles = await call(base, token, 'GET', '/native/presets/absent/roles');
    assertCase(
      results,
      'native.get.native.presets.id.roles',
      'unknown preset is typed 404',
      unknownRoles.status === 404,
      { status: unknownRoles.status },
    );
    const unknownFallback = await call(base, token, 'GET', '/native/presets/absent/fallback');
    assertCase(
      results,
      'native.get.native.presets.id.fallback',
      'unknown preset is typed 404',
      unknownFallback.status === 404,
      { status: unknownFallback.status },
    );
    const unserved = await call(base, token, 'POST', '/native/presets/probe/roles', {
      roleModels: { planner: { provider: 'absent', model: 'm' } },
    });
    assertCase(
      results,
      'native.post.native.presets.id.roles',
      'refuses an unserved binding typed 409',
      unserved.status === 409,
      { status: unserved.status },
    );

    // ------------------------------------------------------- catalog probe
    const connection = await call(base, token, 'POST', '/native/providers/connections', {
      id: 'dead',
      family: 'ollama',
      label: 'dead',
      endpoint: 'http://127.0.0.1:9',
      models: ['llama3:8b'],
      allow_loopback: true,
      connect_timeout_ms: 200,
      total_timeout_ms: 400,
      keep_alive: '30m',
    });
    const probed = await call(base, token, 'POST', '/native/catalog/probe', {
      connectionId: 'dead',
      model: 'llama3:8b',
    });
    assertCase(
      results,
      'native.post.native.catalog.probe',
      'observes a dead endpoint as failed, never ready',
      connection.status === 200 && probed.status === 200 && probed.json?.state === 'failed',
      { connection: connection.status, status: probed.status, state: probed.json?.state },
    );
    const missingProbe = await call(base, token, 'POST', '/native/catalog/probe', {
      connectionId: 'absent',
    });
    assertCase(
      results,
      'native.post.native.catalog.probe',
      'unknown connection is typed 404',
      missingProbe.status === 404,
      { status: missingProbe.status },
    );

    // ----------------------------------------------------------- MCP auth
    const mcp = await call(base, token, 'POST', '/native/mcp/servers', {
      id: 'remote',
      name: 'Remote',
      command: '',
      args: [],
      env: [],
      trusted: true,
      enabled: true,
      transport: 'streamable_http',
      endpoint: 'https://mcp.example.com/rpc',
      timeout_ms: 5000,
      output_cap_bytes: 1048576,
      stderr_cap_bytes: 65536,
    });
    const mcpSecret = `sk-mcp-${randomBytes(12).toString('hex')}`;
    const mcpAuth = await call(base, token, 'POST', '/native/mcp/servers/remote/auth', {
      action: 'connect',
      accessToken: mcpSecret,
      account: 'acct',
      scopes: ['tools.read'],
    });
    assertCase(
      results,
      'native.post.native.mcp.servers.id.auth',
      'connects without round-tripping the token',
      mcp.status === 200 &&
        mcpAuth.status === 200 &&
        mcpAuth.json?.state === 'authorized' &&
        !mcpAuth.text.includes(mcpSecret),
      { mcp: mcp.status, status: mcpAuth.status },
    );
    const mcpAuthAgain = await call(base, token, 'POST', '/native/mcp/servers/remote/auth', {
      action: 'connect',
      accessToken: mcpSecret,
      account: 'acct',
      scopes: ['tools.read'],
    });
    assertCase(
      results,
      'native.post.native.mcp.servers.id.auth',
      'double connect is idempotent and still discloses no token',
      mcpAuthAgain.status === 200 && !mcpAuthAgain.text.includes(mcpSecret),
      { status: mcpAuthAgain.status },
    );
    const mcpRead = await call(base, token, 'GET', '/native/mcp/servers/remote/auth');
    assertCase(
      results,
      'native.get.native.mcp.servers.id.auth',
      'reports hasToken without the token',
      mcpRead.status === 200 && mcpRead.json?.hasToken === true && !mcpRead.text.includes(mcpSecret),
      { status: mcpRead.status },
    );

    // ------------------------------------------------------ review comments
    const commentsPath = `/native/session/${session}/review/comments`;
    const created = await call(base, token, 'POST', commentsPath, {
      path: 'src/lib.rs',
      lineStart: 3,
      lineEnd: 4,
      side: 'new',
      body: 'guard this',
    });
    const commentId = created.json?.comment?.id;
    assertCase(
      results,
      'native.post.native.session.id.review.comments',
      'creates a scoped, revisioned comment',
      created.status === 200 && commentId !== undefined,
      { status: created.status },
    );
    const listedComments = await call(base, token, 'GET', commentsPath);
    assertCase(
      results,
      'native.get.native.session.id.review.comments',
      'lists the created comment with change staleness',
      listedComments.status === 200 &&
        listedComments.json?.comments?.[0]?.comment?.body === 'guard this',
      { status: listedComments.status },
    );
    const patched = await call(base, token, 'PATCH', `${commentsPath}/${commentId}`, {
      resolved: true,
      expectedRevision: 1,
    });
    assertCase(
      results,
      'native.patch.native.session.id.review.comments.comment',
      'updates with expectedRevision',
      patched.status === 200 && patched.json?.resolved === true,
      { status: patched.status },
    );
    const stalePatch = await call(base, token, 'PATCH', `${commentsPath}/${commentId}`, {
      resolved: false,
      expectedRevision: 1,
    });
    assertCase(
      results,
      'native.patch.native.session.id.review.comments.comment',
      'a stale revision is typed 409',
      stalePatch.status === 409,
      { status: stalePatch.status },
    );
    const traversal = await call(base, token, 'POST', commentsPath, {
      path: '../escape',
      lineStart: 1,
      lineEnd: 1,
      side: 'new',
      body: 'x',
    });
    assertCase(
      results,
      'native.post.native.session.id.review.comments',
      'path traversal is refused',
      traversal.status === 400,
      { status: traversal.status },
    );
    const deletedComment = await call(
      base,
      token,
      'DELETE',
      `${commentsPath}/${commentId}?expectedRevision=2`,
    );
    assertCase(
      results,
      'native.delete.native.session.id.review.comments.comment',
      'deletes only the matching revision',
      deletedComment.status === 200,
      { status: deletedComment.status },
    );
    const foreign = await call(base, token, 'GET', `/native/session/${Number(session) + 1000}/review/comments`);
    assertCase(
      results,
      'native.get.native.session.id.review.comments',
      'a foreign session is typed 404',
      foreign.status === 404,
      { status: foreign.status },
    );

    // ------------------------------------------------------ session settings
    const settingsPath = `/native/session/${session}/settings`;
    const settings = await call(base, token, 'PUT', settingsPath, {
      compactionModel: 'p1/m',
      maxTurns: 12,
      maxConsecutiveFailures: 3,
    });
    const settingsRead = await call(base, token, 'GET', settingsPath);
    assertCase(
      results,
      'native.put.native.session.id.settings',
      'applies a validated settings patch',
      settings.status === 200 &&
        Array.isArray(settings.json?.applied) &&
        settings.json.applied.includes('maxTurns') &&
        settings.json.applied.includes('compactionModel'),
      { status: settings.status, applied: settings.json?.applied },
    );
    assertCase(
      results,
      'native.get.native.session.id.settings',
      'reads the durable settings with apply timing',
      settingsRead.status === 200 &&
        settingsRead.json?.maxTurns === 12 &&
        settingsRead.json?.compactionModel === 'p1/m' &&
        settingsRead.json?.applyTiming?.maxTurns === 'next_task',
      { status: settingsRead.status },
    );
    const badSettings = await call(base, token, 'PUT', settingsPath, { maxTurns: 0 });
    assertCase(
      results,
      'native.put.native.session.id.settings',
      'out-of-range value is typed 400',
      badSettings.status === 400,
      { status: badSettings.status },
    );
    const unservedModel = await call(base, token, 'PUT', settingsPath, {
      compactionModel: 'absent/x',
    });
    assertCase(
      results,
      'native.put.native.session.id.settings',
      'unserved compaction model is typed 409',
      unservedModel.status === 409,
      { status: unservedModel.status },
    );

    // ------------------------------------------------------- export / accept
    const exportPath = `/native/session/${session}/export`;
    const exported = await call(base, token, 'POST', exportPath, {
      redact: true,
      maxBytes: 65536,
    });
    assertCase(
      results,
      'native.post.native.session.id.export',
      'exports a bounded bundle with a digest',
      exported.status === 200 &&
        exported.json?.bundle?.schema === 'faktor.session-export/v1' &&
        typeof exported.json?.digest === 'string' &&
        exported.json.digest.length === 64,
      { status: exported.status },
    );
    const accept = await call(base, token, 'POST', `/native/session/${session}/changes/accept`, {
      approved: ['a.txt'],
      rejected: [],
    });
    assertCase(
      results,
      'native.post.native.session.id.changes.accept',
      'without a staged shadow accepts nothing (typed 503/404)',
      (accept.status === 503 && accept.json?.error?.code === 'unavailable') ||
        (accept.status === 404 && accept.json?.error?.code === 'not_found'),
      { status: accept.status, code: accept.json?.error?.code },
    );
    const preview = await call(
      base,
      token,
      'POST',
      `/native/session/${session}/checkpoints/1/restore/preview`,
      { direction: 'undo' },
    );
    assertCase(
      results,
      'native.post.native.session.id.checkpoints.sequence.restore.preview',
      'a missing checkpoint/authority is typed 503/404, never a fake plan',
      (preview.status === 503 && preview.json?.error?.code === 'unavailable') ||
        (preview.status === 404 && preview.json?.error?.code === 'not_found'),
      { status: preview.status, code: preview.json?.error?.code },
    );

    // --------------------------------------------------------- session delete
    const deletePath = `/native/session/${session}`;
    const deletedSession = await call(base, token, 'DELETE', deletePath);
    assertCase(
      results,
      'native.delete.native.session.id',
      'deletes with a tombstone',
      deletedSession.status === 200 &&
        deletedSession.json?.deleted === true &&
        deletedSession.json?.idempotent === false,
      { status: deletedSession.status },
    );
    const replay = await call(base, token, 'DELETE', deletePath);
    assertCase(
      results,
      'native.delete.native.session.id',
      'replay is idempotent',
      replay.status === 200 && replay.json?.idempotent === true,
      { status: replay.status },
    );
    const exportAfter = await call(base, token, 'POST', exportPath, {});
    assertCase(
      results,
      'native.post.native.session.id.export',
      'deleted content refuses export with the tombstone',
      exportAfter.status === 409 && exportAfter.json?.error?.code === 'session_deleted',
      { status: exportAfter.status, code: exportAfter.json?.error?.code },
    );

    const rows = [...results.values()].sort((a, b) => (a.route < b.route ? -1 : 1));
    const expected = [...REV8_ROUTE_IDS, ...CREDENTIAL_ROUTES].sort();
    const missing = expected.filter((route) => !results.has(route));
    for (const route of missing) {
      assertCase(results, route, 'probe present', false, { missing: true });
    }
    const document = {
      schema: 'faktor-programme-rev8-probe/v1',
      generated_at: new Date().toISOString(),
      binary: binaryPath,
      commit: execGit(['rev-parse', 'HEAD']),
      routes: rows.length,
      failed: rows.filter((row) => !row.passed).length,
      missing,
      digest: sha(JSON.stringify(rows)),
      results: rows,
    };
    if (report) {
      mkdirSync(OUT_DIR, { recursive: true });
      writeFileSync(resolve(OUT_DIR, 'rev8-probe.json'), `${JSON.stringify(document, null, 2)}\n`);
    }
    return {
      status: document.failed === 0 && missing.length === 0 ? 'proven' : 'failed',
      results: rows,
      missing,
      document,
    };
  } finally {
    daemon?.child?.kill('SIGKILL');
    if (daemon?.child) {
      await new Promise((done) => {
        if (daemon.child.exitCode !== null) done();
        else daemon.child.once('exit', done);
      });
    }
    rmSync(tmp, { recursive: true, force: true });
  }
}

function execGit(args) {
  try {
    return execFileSync('git', ['-C', ROOT, ...args], {
      encoding: 'utf8',
    }).trim();
  } catch {
    return null;
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  probeRev8({ report: true })
    .then((result) => {
      console.log(
        `rev8 probe: ${result.status} (${result.results?.length ?? 0} routes, ${result.document?.failed ?? '?'} failed, ${result.missing?.length ?? 0} missing)`,
      );
      for (const row of result.results.filter((entry) => !entry.passed)) {
        console.error(`  ${row.route}: ${JSON.stringify(row.cases.filter((c) => !c.passed))}`);
      }
      if (result.status !== 'proven') process.exitCode = 1;
    })
    .catch((error) => {
      console.error(`rev8 probe: ${error.stack ?? error}`);
      process.exitCode = 1;
    });
}
