#!/usr/bin/env node
// Generated auth/tenant matrix probe over EVERY daemon route.
//
// The route list is the programme registry's `native-route` rows, cross-checked
// against the live server source by `checks/route-authority.mjs`. Each route's
// declared principal class/scope (route-authority.json) decides the cells:
//
//   * unauthenticated            -> 401 (worker/webhook credential routes are
//                                   typed credential refusals; the webhook is
//                                   the one explicit public exception)
//   * wrong bearer               -> 401
//   * control route, no token    -> 401 (never `*_disabled`)
//   * control route, bad token   -> 401 (never `*_disabled`)
//   * disabled subsystem         -> the auth refusal is identical to the
//                                   enabled case: no feature-state oracle
//   * daemon bearer              -> not 401 (daemon class) / 401|403|409 for a
//                                   control-plane route that also needs a token
//   * x-faktor-server-password   -> same as the bearer form
//   * control token alone        -> 401 (never a daemon credential)
//   * control token as bearer    -> 401 (never substituted for the password)
//   * session path/query IDOR    -> unknown/foreign session is a typed 400/404
//   * embedded resource IDOR     -> a review comment/attachment created under
//                                   session A is invisible under session B
//   * tenant IDOR (cloud)        -> a valid token for org A against org B's
//                                   path/query/body is the SAME typed 404 a
//                                   missing org answers (no existence leak)
//
// A route cannot be invisible: an undeclared route, a declared route with no
// handler gate, or a registry/source drift fails before any request is sent.

import { execFileSync, spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { normalizeCapabilityId } from '../../../scripts/discover-capabilities.mjs';
import {
  assertAuthOrder,
  assertForeignOrgOrder,
  credentialProbeBody,
  errorCode,
  isOrderingException,
  requiresControlToken,
  typedRefusal,
} from '../checks/auth-order.mjs';
import { scan } from '../checks/route-authority.mjs';
import { cliBinary, request } from './routes.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT_DIR = resolve(ROOT, 'target/certification/programme');
const AUTHORITY_PATH = resolve(ROOT, 'tests/programme/route-authority.json');
const REGISTRY_PATH = resolve(ROOT, 'tests/programme/registry.json');

function sha(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

function authority() {
  return JSON.parse(readFileSync(AUTHORITY_PATH, 'utf8')).entries;
}

function registryRouteIds() {
  const registry = JSON.parse(readFileSync(REGISTRY_PATH, 'utf8'));
  return new Set(
    registry.rows
      .filter((row) => row.kind === 'native-route')
      .map((row) => normalizeCapabilityId(row.id)),
  );
}

async function startDaemon(binary, config, data, password) {
  const child = spawn(binary, ['serve', '--port', '0', '--data-dir', data, '--config', config], {
    cwd: ROOT,
    env: { ...process.env, FAKTOR_SERVER_PASSWORD: password, AUTH_MATRIX_KEY: 'loopback-only' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  child.stdout.on('data', (chunk) => {
    output += chunk.toString();
  });
  child.stderr.on('data', (chunk) => {
    output += chunk.toString();
  });
  const deadline = Date.now() + 30_000;
  let base = null;
  while (Date.now() < deadline) {
    const match = /(http:\/\/127\.0\.0\.1:\d+)/.exec(output);
    if (match) {
      base = match[1];
      break;
    }
    if (child.exitCode !== null) break;
    await new Promise((done) => setTimeout(done, 50));
  }
  if (base === null) {
    child.kill('SIGKILL');
    throw new Error(`auth matrix daemon did not start: ${output.slice(-1500)}`);
  }
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${base}/native/ready`, {
        headers: { authorization: `Bearer ${password}` },
      });
      if (response.ok) return { child, base, output: () => output };
    } catch {
      // loopback poll
    }
    await new Promise((done) => setTimeout(done, 50));
  }
  child.kill('SIGKILL');
  throw new Error(`auth matrix daemon never became ready: ${output.slice(-1500)}`);
}

async function stopDaemon(daemon) {
  if (!daemon?.child) return;
  daemon.child.kill('SIGKILL');
  await new Promise((done) => {
    if (daemon.child.exitCode !== null) done();
    else daemon.child.once('exit', done);
  });
}

function configDocument(cloud) {
  return `${JSON.stringify({
    config_version: 1,
    model: 'm',
    sandbox: { network: [] },
    ...(cloud ? { cloud: { enabled: true } } : {}),
    providers: [
      {
        kind: 'open_ai',
        id: 'p1',
        base_url: 'http://127.0.0.1:9/v1',
        api_key_env: 'AUTH_MATRIX_KEY',
        api: 'chat',
        allow_loopback: true,
        models: ['m'],
      },
    ],
  })}\n`;
}

const SESSION_ID = '{id}';
const FOREIGN_SESSION = '999999';

/** Resolve one route template for a concrete session scope. */
function resolvePath(template, session) {
  return template.replace(/\{([^}]+)\}/g, (_match, name) => {
    if (name === 'id' || name === 'session') return session ?? FOREIGN_SESSION;
    if (name === 'sequence') return '1';
    if (name === 'task_id') return '1';
    if (name === 'digest') return 'a'.repeat(64);
    if (name === 'ref_id') return '1';
    if (name === 'kind') return 'probe';
    if (name === 'key') return 'probe';
    if (name === 'comment') return 'absent-comment';
    if (name === 'child_id' || name === 'child') return 'absent-child';
    if (name === 'run_id') return 'absent-run';
    if (name === 'tournament_id') return 'absent-tournament';
    if (name === 'terminal_id') return 'absent-terminal';
    if (name === 'operation') return 'absent-operation';
    if (name === 'handle') return 'absent-handle';
    if (name === 'scope_key') return 'absent-scope';
    return 'absent';
  });
}

function queryFor(path, session) {
  if (path === '/native/transcript/search') return `?session=${session}&q=probe`;
  if (
    path === '/native/context/manifest' ||
    path === '/native/context/manifests' ||
    path === '/native/context/export' ||
    path === '/native/index/coverage' ||
    path === '/native/memory' ||
    path === '/native/memory/records' ||
    path === '/native/terminals' ||
    path === '/native/agents' ||
    path === '/native/orchestrator/graph' ||
    path === '/native/messages' ||
    path === '/native/events' ||
    path === '/native/permissions'
  ) {
    return `?session=${session}`;
  }
  if (path === '/native/tasks/{id}/proof' || path === '/native/tasks/{id}/completion-steps') {
    return `?session=${session}`;
  }
  if (path === '/native/evidence/{id}' || path === '/native/evidence/{id}/retrieve') {
    return `?session=${session}`;
  }
  return '';
}

function bodyFor(method, path) {
  if (method === 'get' || method === 'delete') return undefined;
  if (path === '/native/orgs') {
    return { name: 'Matrix Bootstrap', owner_email: 'owner@matrix.test', display_name: 'Matrix' };
  }
  // Worker-token routes: a VALID DTO carrying a credential that is not bound
  // to any worker, so the cell tests credential ordering (401), not parsing.
  const credentialBody = credentialProbeBody(path);
  if (credentialBody !== null) return credentialBody;
  if (path === '/native/session/{id}/abort') return { session_id: FOREIGN_SESSION };
  if (path === '/native/permission/reply') {
    return { session_id: FOREIGN_SESSION, permission_id: '999999', decision: 'allow' };
  }
  return {};
}

function isStream(path) {
  return /\/events(\?|$)/.test(path);
}

function admitted(response) {
  return response.aborted || (response.status !== null && response.status !== 401 && response.status !== 403 && response.status < 500);
}

function headerSet(headers) {
  return Object.fromEntries(Object.entries(headers).map(([key, value]) => [key.toLowerCase(), value]));
}

function check(results, route, name, ok, observed) {
  results.cases.push({ route: route.id, name, passed: Boolean(ok), observed });
  if (!ok) results.failures.push({ route: route.id, name, observed });
}

async function call(base, method, path, options = {}) {
  return request(
    base,
    null,
    method.toUpperCase(),
    path,
    options.body,
    options.timeoutMs ?? 5_000,
    options.headers ?? {},
  );
}

/** Phase 1: every route against the plain local daemon (daemon password). */
async function localMatrix(base, password, entries, sessions, results) {
  const daemon = { authorization: `Bearer ${password}` };
  const xHeader = { 'x-faktor-server-password': password };
  const wrong = { authorization: 'Bearer deadbeefdeadbeefdeadbeefdeadbeef' };
  const controlAsBearer = { authorization: `Bearer ${randomBytes(32).toString('hex')}` };

  for (const route of entries) {
    const method = route.method;
    // DELETE /native/session/{id} is the one destructive route: its own-path
    // cell uses a nonexistent id so the matrix never deletes its fixtures.
    const destructive = route.path === '/native/session/{id}' && method === 'delete';
    const ownSession = destructive ? FOREIGN_SESSION : sessions.a;
    const ownPath = `${resolvePath(route.path, ownSession)}${queryFor(route.path, ownSession)}`;
    const body = bodyFor(method, route.path);
    const timeoutMs = isStream(ownPath) ? 900 : 5_000;
    const exception = isOrderingException(method, route.path);
    // The bootstrap route needs its idempotency key to reach the explicit
    // cloud-disabled cell; it is not a credential.
    const cellHeaders = exception === 'bootstrap' ? { 'idempotency-key': 'matrix-bootstrap' } : {};

    const unauth = await call(base, method, ownPath, { body, timeoutMs });
    const wrongAuth = await call(base, method, ownPath, { body, timeoutMs, headers: wrong });
    const bearer = await call(base, method, ownPath, { body, timeoutMs, headers: { ...daemon, ...cellHeaders } });
    const xForm = await call(base, method, ownPath, { body, timeoutMs, headers: { ...xHeader, ...cellHeaders } });
    const foreignBearer = await call(base, method, ownPath, {
      body,
      timeoutMs,
      headers: controlAsBearer,
    });

    const credentialOnly = route.principal === 'worker_token' || route.principal === 'webhook';

    if (credentialOnly) {
      if (route.principal === 'worker_token') {
        // The worker token IS the credential: a missing/unbound token is the
        // same typed 401 an enabled plane answers, even with the plane off.
        const unauthOrder = assertAuthOrder(unauth);
        check(results, route, 'unauthenticated credential is refused before the plane state', unauthOrder.ok, unauthOrder.observed);
        const daemonOrder = assertAuthOrder(bearer);
        check(results, route, 'daemon password is NOT a worker credential', daemonOrder.ok, daemonOrder.observed);
        const wrongOrder = assertAuthOrder(wrongAuth);
        check(results, route, 'wrong bearer is refused before the plane state', wrongOrder.ok, wrongOrder.observed);
      } else {
        // Explicitly public: the HMAC signature cannot be verified without
        // the sink, so its configuration refusal is documented.
        check(
          results,
          route,
          'public webhook is a typed refusal (configuration exception)',
          exception === 'public' && typedRefusal(unauth) && [401, 409].includes(unauth.status),
          { status: unauth.status, code: errorCode(unauth) },
        );
      }
    } else {
      const unauthOrder = assertAuthOrder(unauth);
      check(results, route, 'unauthenticated is 401 before any feature state', unauthOrder.ok, unauthOrder.observed);
      const wrongOrder = assertAuthOrder(wrongAuth);
      check(results, route, 'wrong bearer is 401 before any feature state', wrongOrder.ok, wrongOrder.observed);
      if (route.principal === 'daemon' || route.principal === 'daemon_or_control_org') {
        check(results, route, 'daemon bearer is admitted', admitted(bearer), {
          status: bearer.status,
          aborted: bearer.aborted,
          code: bearer.code,
        });
        check(results, route, 'x-faktor-server-password is admitted', admitted(xForm), {
          status: xForm.status,
          aborted: xForm.aborted,
        });
      } else if (exception === 'bootstrap') {
        // The daemon bootstrap (NO token presented) is the ONE place the
        // explicit `cloud_disabled` configuration state is disclosed.
        check(
          results,
          route,
          'daemon bootstrap sees the typed configuration state',
          bearer.status === 409 && errorCode(bearer) === 'cloud_disabled',
          { status: bearer.status, code: errorCode(bearer) },
        );
        check(
          results,
          route,
          'x-faktor-server-password bootstrap sees the typed configuration state',
          xForm.status === 409 && errorCode(xForm) === 'cloud_disabled',
          { status: xForm.status, code: errorCode(xForm) },
        );
      } else {
        const bearerOrder = assertAuthOrder(bearer);
        check(
          results,
          route,
          'control-plane route without its token is 401 before any feature state',
          bearerOrder.ok,
          bearerOrder.observed,
        );
        const xOrder = assertAuthOrder(xForm);
        check(
          results,
          route,
          'x-faktor-server-password alone is 401 before any feature state',
          xOrder.ok,
          xOrder.observed,
        );
      }
      check(results, route, 'control token as bearer is refused', foreignBearer.status === 401, {
        status: foreignBearer.status,
      });
      // A PRESENTED invalid control token must lose exactly like a missing
      // one, including on every disabled subsystem.
      if (requiresControlToken(route.principal) || exception === 'bootstrap') {
        const invalidControl = await call(base, method, ownPath, {
          body,
          timeoutMs,
          headers: { ...daemon, 'x-faktor-control-token': randomBytes(32).toString('hex'), ...(exception === 'bootstrap' ? { 'idempotency-key': 'matrix-bootstrap' } : {}) },
        });
        const order = assertAuthOrder(invalidControl);
        check(results, route, 'invalid control token is 401 before any feature state', order.ok, order.observed);
      }
    }

    // Owner-only resources addressed by id: an absent id is a typed client
    // refusal, never a 200 empty namespace.
    if (route.scope === 'global' && route.principal === 'daemon' && /\{[^}]+\}/.test(route.path)) {
      check(
        results,
        route,
        'absent owner-only resource id is refused',
        [400, 404, 409].includes(bearer.status),
        { status: bearer.status, code: bearer.code },
      );
    }

    // Session-scoped IDOR: an unknown session is a typed client refusal.
    if (route.scope === 'session_path') {
      const unknown = `${resolvePath(route.path, FOREIGN_SESSION)}${queryFor(route.path, FOREIGN_SESSION)}`;
      const response = await call(base, method, unknown, { body, timeoutMs, headers: daemon });
      check(results, route, 'foreign/unknown session is refused', [400, 404].includes(response.status), {
        status: response.status,
        code: response.code,
      });
      check(results, route, 'own session is admitted', admitted(bearer), {
        status: bearer.status,
        aborted: bearer.aborted,
        code: bearer.code,
      });
      const headerAttack = await call(base, method, ownPath, {
        body,
        timeoutMs,
        headers: {
          ...daemon,
          'x-faktor-session-id': FOREIGN_SESSION,
          'x-faktor-session': FOREIGN_SESSION,
        },
      });
      const leaked =
        headerAttack.text.includes(`"sessionId":"${FOREIGN_SESSION}"`) ||
        headerAttack.text.includes(`"sessionId":${FOREIGN_SESSION}`) ||
        headerAttack.text.includes(`"session_id":"${FOREIGN_SESSION}"`);
      check(results, route, 'session header cannot retarget the path', !leaked && admitted(headerAttack), {
        status: headerAttack.status,
        aborted: headerAttack.aborted,
        leaked,
      });
    } else if (route.scope === 'session_query') {
      const unknown = `${resolvePath(route.path, FOREIGN_SESSION)}${queryFor(route.path, FOREIGN_SESSION)}`;
      const response = await call(base, method, unknown, { body, timeoutMs, headers: daemon });
      check(results, route, 'foreign/unknown session query is refused', [400, 404].includes(response.status), {
        status: response.status,
        code: response.code,
      });
    } else if (route.scope === 'session_child') {
      const response = await call(base, method, resolvePath(route.path), { body, timeoutMs, headers: daemon });
      check(results, route, 'unowned child id is refused', [400, 404, 409].includes(response.status), {
        status: response.status,
        code: response.code,
      });
    }
  }
}

/** Embedded-resource IDOR: objects of session A stay invisible in B. */
async function resourceIdor(base, password, sessions, results) {
  const headers = { authorization: `Bearer ${password}` };
  const route = (id) => ({ id, path: '' });

  const comment = await call(base, 'post', `/native/session/${sessions.a}/review/comments`, {
    headers,
    body: { path: 'src/lib.rs', lineStart: 1, lineEnd: 1, side: 'new', body: 'matrix probe' },
  });
  check(results, route('idor.review-comments'), 'comment created under session A', comment.status === 200, {
    status: comment.status,
  });
  const commentId = comment.json?.comment?.id;
  if (commentId) {
    const listB = await call(base, 'get', `/native/session/${sessions.b}/review/comments`, { headers });
    const leaked = JSON.stringify(listB.json ?? {}).includes(commentId);
    check(results, route('idor.review-comments'), 'session B listing does not contain A comment', !leaked, {
      status: listB.status,
      leaked,
    });
    const patchB = await call(base, 'patch', `/native/session/${sessions.b}/review/comments/${commentId}`, {
      headers,
      body: { resolved: true },
    });
    check(results, route('idor.review-comments'), 'cross-session patch is refused', [400, 404].includes(patchB.status), {
      status: patchB.status,
      code: patchB.code,
    });
    const deleteB = await call(base, 'delete', `/native/session/${sessions.b}/review/comments/${commentId}`, {
      headers,
    });
    check(results, route('idor.review-comments'), 'cross-session delete is refused', [400, 404].includes(deleteB.status), {
      status: deleteB.status,
      code: deleteB.code,
    });
  }

  const payload = Buffer.from('auth-matrix-attachment').toString('base64');
  const upload = await call(base, 'post', `/native/session/${sessions.a}/attachments`, {
    headers,
    body: { mime: 'text/plain', filename: 'probe.txt', data_base64: payload },
  });
  check(results, route('idor.attachments'), 'attachment created under session A', upload.status === 200, {
    status: upload.status,
  });
  const ref = upload.json?.id ?? upload.json?.refId ?? upload.json?.ref_id;
  const digest = upload.json?.digest;
  if (ref !== undefined) {
    for (const suffix of [`ref/${ref}`, `ref/${ref}/bytes`]) {
      const response = await call(base, 'get', `/native/session/${sessions.b}/attachments/${suffix}`, { headers });
      check(results, route('idor.attachments'), `cross-session ${suffix} is refused`, [400, 404].includes(response.status), {
        status: response.status,
        code: response.code,
      });
    }
  }
  if (digest) {
    const response = await call(base, 'get', `/native/session/${sessions.b}/attachments/blob/${digest}`, {
      headers,
    });
    check(results, route('idor.attachments'), 'cross-session blob is refused', [400, 404].includes(response.status), {
      status: response.status,
      code: response.code,
    });
  }

  // Rev8 browser: unknown browser/agent ids are typed 404, never an empty
  // 200 page or a daemon-wide artifact read.
  for (const path of [
    `/native/browser/sessions/${FOREIGN_SESSION}/observations`,
    '/native/browser/artifacts/absent-artifact',
    '/native/browser/artifacts/absent-artifact/bytes',
    `/native/browser/sessions?session=${FOREIGN_SESSION}`,
  ]) {
    const response = await call(base, 'get', path, { headers });
    check(results, route('idor.browser'), `unknown browser scope ${path} is refused`, [400, 404].includes(response.status), {
      status: response.status,
      code: response.code,
    });
  }

  const unknownMeta = await call(base, 'get', `/native/sessions/${FOREIGN_SESSION}/meta`, { headers });
  check(results, route('idor.session-meta'), 'unknown session meta is refused', [400, 404].includes(unknownMeta.status), {
    status: unknownMeta.status,
    code: unknownMeta.code,
  });
  for (const path of [
    `/native/session/${FOREIGN_SESSION}/context/pins`,
    `/native/context/export?session=${FOREIGN_SESSION}`,
    `/native/context/manifest?session=${FOREIGN_SESSION}`,
  ]) {
    const response = await call(base, 'get', path, { headers });
    check(results, route('idor.context-scope'), `unknown session ${path} is refused`, [400, 404].includes(response.status), {
      status: response.status,
      code: response.code,
    });
  }
}

/** Phase 2: cloud control plane, two tenants, cross-org IDOR. */
async function cloudMatrix(base, password, entries, results) {
  const daemon = { authorization: `Bearer ${password}` };
  const boot = async (name, email, key) => {
    const response = await call(base, 'post', '/native/orgs', {
      headers: { ...daemon, 'idempotency-key': key },
      body: { name, owner_email: email, display_name: name },
    });
    if (response.status !== 200 || !response.json?.token) {
      throw new Error(`bootstrap ${name}: ${response.status} ${response.text.slice(0, 300)}`);
    }
    const organization = response.json.organization;
    return {
      id: typeof organization === 'string' ? organization : organization?.id,
      token: response.json.token,
      email,
    };
  };
  const a = await boot('Matrix A', 'owner-a@matrix.test', 'matrix-boot-a');
  const b = await boot('Matrix B', 'owner-b@matrix.test', 'matrix-boot-b');
  const authA = { ...daemon, 'x-faktor-control-token': a.token };
  const authB = { ...daemon, 'x-faktor-control-token': b.token };
  const tokenOnly = { 'x-faktor-control-token': a.token };

  const identityA = await call(base, 'get', '/native/identity', { headers: authA });
  const identityB = await call(base, 'get', '/native/identity', { headers: authB });
  check(results, { id: 'tenant.identity' }, 'token A resolves to tenant A', identityA.json?.identity?.organization === a.id, {
    status: identityA.status,
    organization: identityA.json?.identity?.organization,
  });
  check(results, { id: 'tenant.identity' }, 'token B resolves to tenant B', identityB.json?.identity?.organization === b.id, {
    status: identityB.status,
    organization: identityB.json?.identity?.organization,
  });
  const noDaemon = await call(base, 'get', '/native/identity', { headers: tokenOnly });
  check(results, { id: 'tenant.identity' }, 'control token alone is not the daemon credential', noDaemon.status === 401, {
    status: noDaemon.status,
  });

  // Every organization-scoped route: A's principal alone is admitted (typed
  // result, never 401); the local daemon password without a token is not.
  for (const route of entries.filter((entry) => entry.principal === 'daemon_control' || entry.principal === 'daemon_control_token')) {
    if (route.path === '/native/orgs' && route.method === 'post') continue;
    const path = `${resolvePath(route.path, '1')}${queryFor(route.path, '1')}`;
    const body = bodyFor(route.method, route.path);
    const response = await call(base, route.method, path, {
      headers: authA,
      body,
      timeoutMs: isStream(path) ? 900 : 5_000,
    });
    check(results, route, 'control principal is admitted (typed result)', admitted(response), {
      status: response.status,
      aborted: response.aborted,
      code: response.code,
    });
    const daemonOnly = await call(base, route.method, path, {
      headers: daemon,
      body,
      timeoutMs: isStream(path) ? 900 : 5_000,
    });
    const daemonOrder = assertAuthOrder(daemonOnly);
    check(
      results,
      route,
      'daemon password alone is 401 before any feature state',
      daemonOrder.ok,
      daemonOrder.observed,
    );
    const invalidControl = await call(base, route.method, path, {
      headers: { ...daemon, 'x-faktor-control-token': randomBytes(32).toString('hex') },
      body,
      timeoutMs: isStream(path) ? 900 : 5_000,
    });
    const invalidOrder = assertAuthOrder(invalidControl);
    check(
      results,
      route,
      'invalid control token is 401 before any feature state',
      invalidOrder.ok,
      invalidOrder.observed,
    );
  }

  // Tenant IDOR cells: exactly the typed 404 a missing tenant answers.
  const cross = [
    ['get', `/native/orgs/${b.id}/members`],
    ['post', `/native/orgs/${b.id}/members`],
    ['get', `/native/usage?org=${b.id}`],
    ['get', `/native/outcomes?org=${b.id}`],
    ['get', `/native/usage?org=${a.id}`],
  ];
  const bodies = [
    undefined,
    { email: 'intruder@matrix.test', role: 'viewer' },
    undefined,
    undefined,
    undefined,
  ];
  for (let index = 0; index < cross.length; index += 1) {
    const [method, path] = cross[index];
    const response = await call(base, method, path, {
      headers: { ...authA, 'idempotency-key': 'matrix-cross-org' },
      body: bodies[index],
    });
    const isOwnOrg = path.includes(`org=${a.id}`);
    if (isOwnOrg) {
      check(results, { id: 'tenant.usage' }, 'own-org usage is not a tenant refusal', response.status !== 404, {
        status: response.status,
        code: response.code,
      });
    } else {
      // Same status/code as a tenant that never existed: foreign existence is
      // not observable from the refusal, and no disabled/plan state may lead.
      const missingOrg = `org_${'0'.repeat(32)}`;
      const missing = await call(base, method, path.replaceAll(b.id, missingOrg), {
        headers: { ...authA, 'idempotency-key': 'matrix-missing-org' },
        body: bodies[index],
      });
      const order = assertForeignOrgOrder(response, missing, [b.id, b.email]);
      check(
        results,
        { id: 'tenant.cross-org' },
        `${method.toUpperCase()} ${path.split('?')[0]} foreign org is the same typed 404 as a missing org`,
        order.ok,
        order.observed,
      );
    }
  }

  // Body-named organization (SSO) is bound to the principal, never trusted:
  // a foreign org is the SAME 404 a missing one answers (no SSO-existence or
  // disabled oracle), even though the SSO authority is disabled here.
  const missingOrg = `org_${'0'.repeat(32)}`;
  for (const [route, makeBody] of [
    ['/native/sso/start', (org) => ({ organization: org, redirect_uri: 'http://127.0.0.1/callback' })],
    ['/native/sso/callback', (org) => ({
      organization: org,
      redirect_uri: 'http://127.0.0.1/callback',
      code: 'c',
      state: 's',
    })],
  ]) {
    const ssoCross = await call(base, 'post', route, { headers: authA, body: makeBody(b.id) });
    const ssoMissing = await call(base, 'post', route, { headers: authA, body: makeBody(missingOrg) });
    const order = assertForeignOrgOrder(ssoCross, ssoMissing, [b.id, b.email]);
    check(results, { id: 'tenant.sso' }, `${route} foreign org is the same 404 as a missing org`, order.ok, order.observed);
  }

  // Approval IDOR: create in B, decide with A.
  const created = await call(base, 'post', '/native/approvals', {
    headers: { ...authB, 'idempotency-key': 'matrix-approval-b' },
    body: { action: 'repository_write', resource: 'repo:b', reason: 'matrix' },
  });
  check(results, { id: 'tenant.approvals' }, 'tenant B can request an approval', created.status === 201, {
    status: created.status,
  });
  const approvalId = created.json?.approval?.id;
  if (approvalId) {
    const decided = await call(base, 'post', `/native/approvals/${approvalId}/decide`, {
      headers: { ...authA, 'idempotency-key': 'matrix-approval-a' },
      body: { approved: true, note: 'cross-tenant' },
    });
    check(results, { id: 'tenant.approvals' }, 'tenant A cannot decide tenant B approval', decided.status === 404, {
      status: decided.status,
      code: decided.code,
    });
    const listA = await call(base, 'get', '/native/approvals', { headers: authA });
    check(
      results,
      { id: 'tenant.approvals' },
      'tenant A listing excludes tenant B approval',
      !JSON.stringify(listA.json ?? {}).includes(approvalId),
      { status: listA.status },
    );
  }

  return { a, b };
}

/** Per-request auth cost: p50/p95 over sequential authenticated reads. */
async function measureAuth(base, password, results) {
  const headers = { authorization: `Bearer ${password}` };
  const samples = [];
  const rounds = 120;
  for (let index = 0; index < rounds; index += 1) {
    const started = process.hrtime.bigint();
    const response = await call(base, 'get', '/native/health', { headers });
    const micros = Number(process.hrtime.bigint() - started) / 1000;
    if (response.status !== 200) throw new Error(`auth cost probe: ${response.status}`);
    samples.push(micros);
  }
  samples.sort((left, right) => left - right);
  const at = (fraction) => samples[Math.min(samples.length - 1, Math.floor(samples.length * fraction))];
  const p50 = at(0.5);
  const p95 = at(0.95);
  const mean = samples.reduce((sum, value) => sum + value, 0) / samples.length;
  check(results, { id: 'perf.auth-check' }, 'library auth stays a bounded constant-time check (no scan)', p95 < 25_000, {
    rounds,
    p50_us: Number(p50.toFixed(1)),
    p95_us: Number(p95.toFixed(1)),
    mean_us: Number(mean.toFixed(1)),
  });
  return { rounds, p50_us: Number(p50.toFixed(1)), p95_us: Number(p95.toFixed(1)), mean_us: Number(mean.toFixed(1)) };
}

export async function probeAuthMatrix({ report = false } = {}) {
  const staticCheck = scan({ root: ROOT });
  if (!staticCheck.passed) {
    return {
      status: 'failed',
      failures: staticCheck.observed.errors.map((error) => ({ id: 'static.route-authority', error })),
      cases: [],
      static: staticCheck.observed,
    };
  }
  const entries = authority();
  const registryIds = registryRouteIds();
  const declared = new Set(entries.map((entry) => entry.id));
  const invisible = [...registryIds].filter((id) => !declared.has(id));
  const stale = [...declared].filter((id) => !registryIds.has(id));
  if (invisible.length > 0 || stale.length > 0) {
    return {
      status: 'failed',
      failures: [
        ...invisible.map((id) => ({ id, error: 'registry route has no authority declaration' })),
        ...stale.map((id) => ({ id, error: 'authority declaration has no registry route' })),
      ],
      cases: [],
      static: staticCheck.observed,
    };
  }

  const binary = cliBinary();
  if (binary === null) {
    return { status: 'deferred', reason: 'no faktor-cli binary; trusted lane builds it', cases: [] };
  }

  const tmp = mkdtempSync(join(tmpdir(), 'faktor-auth-matrix-'));
  const data = join(tmp, 'data');
  const workspace = join(tmp, 'workspace');
  const config = join(tmp, 'faktor.json');
  mkdirSync(data, { recursive: true });
  mkdirSync(workspace, { recursive: true });
  writeFileSync(config, configDocument(false));

  const password = randomBytes(32).toString('hex');
  const results = { cases: [], failures: [] };
  let daemon = null;
  let cloudDaemon = null;
  let sessions = null;
  let tenants = null;
  let perf = null;
  try {
    daemon = await startDaemon(binary, config, data, password);
    const create = async (title) => {
      const response = await call(daemon.base, 'post', '/native/session', {
        headers: { authorization: `Bearer ${password}` },
        body: { provider: 'p1', model: 'm', workspace, title },
      });
      if (response.status !== 200 || !response.json?.id) {
        throw new Error(`create session ${title}: ${response.status} ${response.text.slice(0, 300)}`);
      }
      return String(response.json.id);
    };
    sessions = { a: await create('matrix A'), b: await create('matrix B') };
    await localMatrix(daemon.base, password, entries, sessions, results);
    await resourceIdor(daemon.base, password, sessions, results);
    perf = await measureAuth(daemon.base, password, results);
  } finally {
    await stopDaemon(daemon);
    rmSync(tmp, { recursive: true, force: true });
  }

  const cloudTmp = mkdtempSync(join(tmpdir(), 'faktor-auth-matrix-cloud-'));
  const cloudData = join(cloudTmp, 'data');
  const cloudWorkspace = join(cloudTmp, 'workspace');
  const cloudConfig = join(cloudTmp, 'faktor.json');
  mkdirSync(cloudData, { recursive: true });
  mkdirSync(cloudWorkspace, { recursive: true });
  writeFileSync(cloudConfig, configDocument(true));
  try {
    cloudDaemon = await startDaemon(binary, cloudConfig, cloudData, password);
    tenants = await cloudMatrix(cloudDaemon.base, password, entries, results);
  } finally {
    await stopDaemon(cloudDaemon);
    rmSync(cloudTmp, { recursive: true, force: true });
  }

  const declaredIds = new Set(entries.map((entry) => entry.id));
  const routesCovered = new Set(
    results.cases.map((entry) => entry.route).filter((id) => declaredIds.has(id)),
  );
  const syntheticCases = results.cases.filter((entry) => !declaredIds.has(entry.route)).length;
  // The shared ordering rule's evidence: every invalid/foreign-principal cell
  // that proved the auth refusal came before any feature/tenant state.
  const orderCases = results.cases.filter((entry) =>
    /before any feature state|before the plane state|same typed 404/.test(entry.name),
  ).length;
  const document = {
    schema: 'faktor-auth-tenant-matrix/v1',
    generated_at: new Date().toISOString(),
    binary,
    commit: git(['rev-parse', 'HEAD']),
    routes: entries.length,
    route_cases: results.cases.length,
    synthetic_cases: syntheticCases,
    routes_covered: routesCovered.size,
    auth_order_cases: orderCases,
    failures: results.failures.length,
    sessions,
    tenants: tenants ? { a: tenants.a.id, b: tenants.b.id } : null,
    perf,
    digest: sha(JSON.stringify(results.cases)),
    cases: results.cases,
    failed: results.failures,
  };
  if (report) {
    mkdirSync(OUT_DIR, { recursive: true });
    writeFileSync(resolve(OUT_DIR, 'auth-matrix.json'), `${JSON.stringify(document, null, 2)}\n`);
  }
  return {
    status: results.failures.length === 0 ? 'proven' : 'failed',
    results: results.cases,
    failures: results.failures,
    document,
    static: staticCheck.observed,
    perf,
  };
}

function git(args) {
  try {
    return execFileSync('git', ['-C', ROOT, ...args], { encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
}

if (import.meta.url === `file://${process.argv[1]}`) {
  probeAuthMatrix({ report: true })
    .then((result) => {
      console.log(
        `auth matrix: ${result.status} (${result.document?.routes ?? 0} routes, ${result.document?.route_cases ?? 0} cases, ${result.failures?.length ?? 0} failed)`,
      );
      if (result.perf) {
        console.log(
          `auth cost: p50 ${result.perf.p50_us}us p95 ${result.perf.p95_us}us over ${result.perf.rounds} requests`,
        );
      }
      if (result.status === 'failed') {
        for (const failure of result.failures.slice(0, 40)) {
          console.error(`  ${failure.route}: ${failure.name} ${JSON.stringify(failure.observed)}`);
        }
        process.exitCode = 1;
      }
    })
    .catch((error) => {
      console.error(`auth matrix: ${error.stack ?? error}`);
      process.exitCode = 1;
    });
}
