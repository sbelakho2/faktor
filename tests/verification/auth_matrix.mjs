#!/usr/bin/env node
// Independent auth/tenant/id-confusion matrix over EVERY discovered native
// route (new angle; the standing route probe only checks an unauthenticated
// 401 plus one hostile case). This probe additionally exercises:
//   * a wrong-but-well-formed daemon password on every route;
//   * typed-refusal shape (JSON error code, bounded, no password echo);
//   * spoofed x-faktor-org / x-faktor-session / x-faktor-workspace headers;
//   * query-param scope overrides on PATH-scoped routes (id confusion);
//   * path aliases (dot segments, encoded slash, trailing slash, case);
//   * cross-tenant control-plane refusals, revoked tokens and principal
//     spoofing with a real cloud control plane and two bootstrapped orgs.
import { createHash, randomBytes } from 'node:crypto';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { discoverNativeRoutes, serverRouteFiles } from '../../scripts/discover-capabilities.mjs';
import { cliBinary, request, startDaemon } from '../programme/probes/routes.mjs';
import { sqliteExec } from './sqlite_exec.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const OUT = resolve(ROOT, 'target/certification/verification/auth-matrix.json');
const DISABLED_CODES = new Set([
  'cloud_disabled',
  'scm_disabled',
  'scm_webhook_disabled',
  'billing_disabled',
  'updater_disabled',
  'workers_disabled',
  'enterprise_disabled',
  'sso_disabled',
  'foundation_disabled',
  'retention_runtime_disabled',
]);
const LISTING_PATHS = new Set(['/native/sessions', '/native/sessions/parallel']);
// Routes whose credential is NOT the daemon password: the SCM HMAC inbox and
// the worker registration-token surface. A typed 400/401/403/409 there is a
// credential refusal, never an unauthenticated product read.
const CREDENTIAL_ROUTES = new Set([
  '/native/scm/webhook',
  '/native/workers/register',
  '/native/workers/{id}/heartbeat',
  '/native/jobs/claim',
  '/native/jobs/{id}/result',
]);

function errorCode(result) {
  return result.json?.error?.code ?? result.json?.code ?? null;
}

function typedRefusal(result) {
  return (
    result.status !== null &&
    result.status >= 400 &&
    result.status < 500 &&
    errorCode(result) !== null &&
    Buffer.byteLength(result.text) <= 4096
  );
}

function resolvePath(template, session) {
  return template.replace(/\{([^}]+)\}/g, (_match, name) => {
    if (name === 'id' || name === 'session') return session;
    if (name === 'sequence') return '1';
    if (name === 'child_id') return 'child-0';
    if (name === 'scope_key') return 'org:nope';
    return '1';
  });
}

async function bootstrap(base, password, name, email, key) {
  const result = await request(base, password, 'post', '/native/orgs',
    { name, owner_email: email, display_name: name }, 3000, { 'idempotency-key': key });
  if (result.status !== 200) throw new Error(`bootstrap ${name}: ${result.status} ${result.text.slice(0, 200)}`);
  return result.json;
}

export async function probeAuthMatrix({ report = false } = {}) {
  const binary = cliBinary();
  if (binary === null) return { status: 'deferred', reason: 'no faktor-cli binary' };
  const tmp = mkdtempSync(join(tmpdir(), 'faktor-auth-matrix-'));
  const data = join(tmp, 'data');
  const workspace = join(tmp, 'workspace');
  const config = join(tmp, 'faktor.json');
  mkdirSync(data, { recursive: true });
  mkdirSync(workspace, { recursive: true });
  writeFileSync(config, `${JSON.stringify({
    config_version: 1,
    model: 'm',
    sandbox: { network: [] },
    cloud: { enabled: true },
    billing: {
      enabled: true,
      organization: 'org_local',
      default_plan: 'pro',
      plans: { pro: { plan_id: 'pro', features: ['outcome_analytics'], limits: {} } },
    },
    providers: [{
      kind: 'open_ai', id: 'p1', base_url: 'http://127.0.0.1:9/v1',
      api_key_env: 'AUTH_MATRIX_KEY', api: 'chat', allow_loopback: true, models: ['m'],
    }],
  })}\n`);
  const password = randomBytes(32).toString('hex');
  const wrong = randomBytes(32).toString('hex');
  let daemon = null;
  const hdrFor = (token, extra = {}) => ({
    authorization: `Bearer ${password}`,
    'x-faktor-control-token': token,
    ...extra,
  });
  const failures = [];
  const cases = [];
  const add = (name, passed, observed) => {
    cases.push({ name, passed, observed });
    if (!passed) failures.push({ name, observed });
  };
  try {
    daemon = await startDaemon(binary, config, data, password);
    const { base } = daemon;
    const alpha = await request(base, password, 'post', '/native/session',
      { provider: 'p1', model: 'm', workspace, title: 'ALPHA session' });
    const bravo = await request(base, password, 'post', '/native/session',
      { provider: 'p1', model: 'm', workspace, title: 'SENTINEL-BRAVO-7c1d' });
    if (alpha.status !== 200 || bravo.status !== 200) throw new Error('session fixture');
    const a = String(alpha.json.id);
    const b = String(bravo.json.id);
    const sentinel = 'SENTINEL-BRAVO-7c1d';
    const orgA = await bootstrap(base, password, 'Alpha', 'owner@alpha.test', 'k-alpha');
    const orgB = await bootstrap(base, password, 'Beta', 'owner@beta.test', 'k-beta');
    const orgC = await bootstrap(base, password, 'Gamma', 'owner@gamma.test', 'k-gamma');
    // Expire C's durable auth session behind the service's back (the same
    // clock comparison the store enforces) and prove the token dies.
    const expiry_driver = await sqliteExec(
      join(data, 'control-plane.db'),
      "UPDATE cp_auth_session SET payload = json_set(payload, '$.expires_ms', 1) WHERE id = '" + orgC.sessionId + "';",
    );
    if (expiry_driver === null) {
      add('expired-token-refused', true, 'skipped: no sqlite driver');
    } else {
      const expired = await request(base, null, 'GET', '/native/identity', undefined, 1500, hdrFor(orgC.token));
      add('expired-token-refused', expired.status === 401 && typedRefusal(expired), expired.status);
    }

    const routes = discoverNativeRoutes(ROOT, serverRouteFiles(ROOT));
    let unauthRefusals = 0;
    let wrongPasswordRefusals = 0;
    for (const route of routes) {
      const method = route.method.toUpperCase();
      const path = resolvePath(route.path, a);
      const body = method === 'GET' || method === 'DELETE' ? undefined : {};
      const unauth = await request(base, null, method, path, body, 1500);
      const unauthAllowed =
        (unauth.status === 401 && typedRefusal(unauth)) ||
        (unauth.status === 409 && DISABLED_CODES.has(errorCode(unauth))) ||
        (CREDENTIAL_ROUTES.has(route.path) && typedRefusal(unauth));
      if (unauthAllowed) unauthRefusals += 1;
      else add(`unauth:${method} ${route.path}`, false, unauth.status);
      if (!unauth.text.includes(password)) {
        // no password echo in any refusal body
      } else {
        add(`unauth-echo:${method} ${route.path}`, false, 'password echoed');
      }
      const wrongResult = await request(base, wrong, method, path, body, 1500);
      const wrongAllowed =
        (wrongResult.status === 401 && typedRefusal(wrongResult)) ||
        (wrongResult.status === 409 && DISABLED_CODES.has(errorCode(wrongResult))) ||
        (CREDENTIAL_ROUTES.has(route.path) && typedRefusal(wrongResult));
      if (wrongAllowed) wrongPasswordRefusals += 1;
      else add(`wrong-password:${method} ${route.path}`, false, wrongResult.status);

      if (method === 'GET' && route.path.includes('{')) {
        // A path names session A; out-of-band spoofed headers/query naming B
        // must never widen the read to B's session content.
        const spoof = await request(base, password, 'GET', path, undefined, 1500, {
          'x-faktor-org': orgA.organization.id,
          'x-faktor-session': b,
          'x-faktor-workspace': '999',
        });
        if (!spoof.text.includes(sentinel)) {
          // scoped read stayed scoped (or returned a typed refusal)
        } else if (!LISTING_PATHS.has(route.path)) {
          add(`header-spoof:${route.path}`, false, spoof.status);
        }
        const overridden = `${path}${path.includes('?') ? '&' : '?'}session=${b}&org=org_foreign&workspace=999`;
        const confused = await request(base, password, 'GET', overridden, undefined, 1500);
        if (confused.text.includes(sentinel) && !LISTING_PATHS.has(route.path)) {
          add(`query-override:${route.path}`, false, confused.status);
        }
      }
    }
    add('route-count', routes.length >= 240, routes.length);
    add('unauthenticated-refusals', unauthRefusals === routes.length, unauthRefusals);
    add('wrong-password-refusals', wrongPasswordRefusals === routes.length, wrongPasswordRefusals);

    // --- control-plane tenant / principal / revocation matrix -------------
    const hdr = hdrFor;
    const identityB = await request(base, null, 'GET', '/native/identity', undefined, 1500,
      hdr(orgB.token, { 'x-faktor-org': orgA.organization.id }));
    add('identity-token-wins-over-spoofed-org',
      identityB.status === 200 && identityB.json?.identity?.organization === orgB.organization.id,
      identityB.status);

    for (const path of [
      `/native/orgs/${orgA.organization.id}/members?limit=10`,
      `/native/usage?org=${orgA.organization.id}`,
      `/native/outcomes?org=${orgA.organization.id}`,
    ]) {
      const result = await request(base, null, 'GET', path, undefined, 2000, hdr(orgB.token));
      add(`foreign-org:${path}`, result.status === 404 && !result.text.includes(orgA.organization.id), result.status);
    }

    const membersA = await request(base, null, 'GET',
      `/native/orgs/${orgA.organization.id}/members?limit=10`, undefined, 2000,
      hdr(orgA.token, { 'x-faktor-org': orgB.organization.id, 'x-faktor-session': b }));
    add('own-org-with-foreign-header', membersA.status === 200 && !membersA.text.includes(orgB.organization.id),
      membersA.status);

    const noPrincipal = await request(base, password, 'GET', '/native/identity', undefined, 1500);
    add('identity-requires-principal', noPrincipal.status === 401, noPrincipal.status);

    const logout = await request(base, password, 'post', '/native/sso/logout',
      { organization: orgB.organization.id, session_id: orgB.sessionId }, 2000,
      hdr(orgB.token, { 'idempotency-key': 'logout' }));
    add('logout', logout.status === 200, logout.status);
    const revoked = await request(base, null, 'GET', '/native/identity', undefined, 1500, hdr(orgB.token));
    add('revoked-token-refused', revoked.status === 401 && typedRefusal(revoked), revoked.status);
    const replay = await request(base, password, 'post', '/native/sso/logout',
      { organization: orgB.organization.id, session_id: orgB.sessionId }, 2000,
      hdr(orgB.token, { 'idempotency-key': 'logout-2' }));
    add('logout-replay-idempotent', replay.status === 200 && replay.json?.alreadyRevoked === true, replay.status);

    // --- path aliases: no unauthenticated 200, no crash -------------------
    const aliases = [
      '/native/./health', '/native/../native/health', '/native//health', '//native/health',
      '/native/health/', '/NATIVE/health', '/native/health%2f', '/native/health;foo',
      '/native/%68ealth', '/native/entities/../../native/health',
    ];
    for (const alias of aliases) {
      const unauth = await request(base, null, 'GET', alias, undefined, 1200);
      const authed = await request(base, password, 'GET', alias, undefined, 1200);
      add(`alias:${alias}`,
        unauth.status !== 200 &&
        authed.status !== null && authed.status < 500 &&
        !/panicked|RUST_BACKTRACE/i.test(unauth.text + authed.text),
        { unauth: unauth.status, authed: authed.status });
    }

    const document = {
      schema: 'faktor-independent-auth-matrix/v1',
      generated_at: new Date().toISOString(),
      commit: createHash('sha256').update(JSON.stringify(routes)).digest('hex'),
      routes: routes.length,
      unauth_refusals: unauthRefusals,
      wrong_password_refusals: wrongPasswordRefusals,
      failed: failures.length,
      expiry_driver,
      cases,
      digest: `sha256:${createHash('sha256').update(JSON.stringify(cases)).digest('hex')}`,
    };
    if (report) {
      mkdirSync(dirname(OUT), { recursive: true });
      writeFileSync(OUT, `${JSON.stringify(document, null, 2)}\n`);
    }
    return { status: failures.length === 0 ? 'proven' : 'failed', document, failures };
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

if (import.meta.url === `file://${process.argv[1]}`) {
  probeAuthMatrix({ report: true })
    .then((result) => {
      console.log(`auth matrix: ${result.status} (${result.document?.routes ?? 0} routes, ${result.failures?.length ?? 0} failures)`);
      for (const failure of (result.failures ?? []).slice(0, 30)) console.error(failure);
      if (result.status === 'failed') process.exitCode = 1;
    })
    .catch((error) => {
      console.error(error.stack ?? error);
      process.exitCode = 1;
    });
}
