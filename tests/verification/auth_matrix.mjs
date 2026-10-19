#!/usr/bin/env node
// Independent auth/tenant/id-confusion matrix over EVERY discovered native
// route (new angle; the standing route probe only checks an unauthenticated
// 401 plus one hostile case). This probe additionally exercises:
//   * a wrong-but-well-formed daemon password on every route;
//   * typed-refusal shape (JSON error code, bounded, no password echo);
//   * the SHARED ordering rule (`checks/auth-order.mjs`): an invalid/foreign
//     principal is refused before any `*_disabled`/plan/tenant state, both on
//     enabled and disabled subsystems (missing AND bogus control tokens);
//   * spoofed x-faktor-org / x-faktor-session / x-faktor-workspace headers;
//   * query-param scope overrides on PATH-scoped routes (id confusion);
//   * path aliases (dot segments, encoded slash, trailing slash, case);
//   * cross-tenant control-plane refusals, revoked tokens and principal
//     spoofing with a real cloud control plane and two bootstrapped orgs.
import { createHash, randomBytes } from 'node:crypto';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  discoverNativeRoutes,
  normalizeCapabilityId,
  serverRouteFiles,
} from '../../scripts/discover-capabilities.mjs';
import {
  assertAuthOrder,
  assertForeignOrgOrder,
  BOOTSTRAP_ROUTES,
  credentialProbeBody,
  errorCode,
  PUBLIC_ROUTES,
  requiresControlToken,
  typedRefusal,
  WORKER_CREDENTIAL_ROUTES,
} from '../programme/checks/auth-order.mjs';
import { cliBinary, request, startDaemon } from '../programme/probes/routes.mjs';
import { sqliteExec } from './sqlite_exec.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const OUT = resolve(ROOT, 'target/certification/verification/auth-matrix.json');
const AUTHORITY_PATH = resolve(ROOT, 'tests/programme/route-authority.json');
const LISTING_PATHS = new Set(['/native/sessions', '/native/sessions/parallel']);

function routePrincipals() {
  const document = JSON.parse(readFileSync(AUTHORITY_PATH, 'utf8'));
  return new Map(
    document.entries.map((entry) => [normalizeCapabilityId(entry.id), entry.principal]),
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
  return { ...result.json, owner_email: email };
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
    const principals = routePrincipals();
    let unauthRefusals = 0;
    let wrongPasswordRefusals = 0;
    let disabledSubsystemOrderCells = 0;
    let invalidControlRefusals = 0;
    for (const route of routes) {
      const method = route.method.toUpperCase();
      const path = resolvePath(route.path, a);
      const principal = principals.get(normalizeCapabilityId(route.id)) ?? null;
      const workerCredential = WORKER_CREDENTIAL_ROUTES.has(route.path);
      const publicRoute = PUBLIC_ROUTES.has(route.path);
      const bootstrap = BOOTSTRAP_ROUTES.has(route.path) && method === 'POST';
      // A valid-shaped body carrying an UNBOUND worker credential, so the
      // cell measures credential ordering, not strict-DTO parsing.
      const body = workerCredential
        ? credentialProbeBody(route.path)
        : bootstrap
          ? { name: 'Matrix Bootstrap', owner_email: 'owner@matrix.test', display_name: 'Matrix' }
          : method === 'GET' || method === 'DELETE'
            ? undefined
            : {};

      const unauth = await request(base, null, method, path, body, 1500);
      const unauthOrder = assertAuthOrder(unauth);
      const unauthAllowed = publicRoute
        ? unauth.status >= 400 && unauth.status < 500 && errorCode(unauth) !== null
        : unauthOrder.ok;
      if (unauthAllowed) unauthRefusals += 1;
      else add(`unauth:${method} ${route.path}`, false, unauthOrder.observed);
      if (unauth.text.includes(password)) {
        add(`unauth-echo:${method} ${route.path}`, false, 'password echoed');
      }
      const wrongResult = await request(base, wrong, method, path, body, 1500);
      const wrongOrder = assertAuthOrder(wrongResult);
      const wrongAllowed = publicRoute
        ? wrongResult.status >= 400 && wrongResult.status < 500 && errorCode(wrongResult) !== null
        : wrongOrder.ok;
      if (wrongAllowed) wrongPasswordRefusals += 1;
      else add(`wrong-password:${method} ${route.path}`, false, wrongOrder.observed);

      // Disabled subsystems: the SAME route with a valid daemon password and
      // a MISSING or BOGUS control credential must answer the auth refusal,
      // never `*_disabled`. This is the ordering cell the old matrix lacked.
      if (requiresControlToken(principal) || bootstrap) {
        disabledSubsystemOrderCells += 1;
        for (const control of [null, 'bogus-control-token']) {
          const headers = {
            authorization: `Bearer ${password}`,
            ...(control === null ? {} : { 'x-faktor-control-token': control }),
            ...(bootstrap ? { 'idempotency-key': 'matrix-bootstrap-order' } : {}),
          };
          const result = await request(base, null, method, path, body, 1500, headers);
          if (bootstrap && control === null) {
            // The explicit daemon bootstrap is the ONE documented exception:
            // with NO token presented it may bootstrap (200 on a wired cloud)
            // or answer the typed `cloud_disabled` configuration refusal.
            const ok =
              result.status === 200 ||
              (result.status === 409 && errorCode(result) === 'cloud_disabled');
            if (ok) invalidControlRefusals += 1;
            else add(`bootstrap-order:${method} ${route.path}`, false, { status: result.status, code: errorCode(result) });
          } else {
            const order = assertAuthOrder(result);
            if (order.ok) invalidControlRefusals += 1;
            else add(`control-order:${method} ${route.path}:${control ?? 'missing'}`, false, order.observed);
          }
        }
      }

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
    add('disabled-subsystem-order-cells', disabledSubsystemOrderCells >= 38, disabledSubsystemOrderCells);
    add('invalid-control-refusals', invalidControlRefusals === disabledSubsystemOrderCells * 2,
      invalidControlRefusals);

    // --- control-plane tenant / principal / revocation matrix -------------
    const hdr = hdrFor;
    const identityB = await request(base, null, 'GET', '/native/identity', undefined, 1500,
      hdr(orgB.token, { 'x-faktor-org': orgA.organization.id }));
    add('identity-token-wins-over-spoofed-org',
      identityB.status === 200 && identityB.json?.identity?.organization === orgB.organization.id,
      identityB.status);

    const missingOrg = `org_${'0'.repeat(32)}`;
    for (const template of [
      `/native/orgs/${orgA.organization.id}/members?limit=10`,
      `/native/usage?org=${orgA.organization.id}`,
      `/native/outcomes?org=${orgA.organization.id}`,
    ]) {
      const result = await request(base, null, 'GET', template, undefined, 2000, hdr(orgB.token));
      const missing = await request(base, null, 'GET',
        template.replaceAll(orgA.organization.id, missingOrg), undefined, 2000, hdr(orgB.token));
      const order = assertForeignOrgOrder(result, missing, [orgA.organization.id, orgA.owner_email]);
      add(`foreign-org:${template.split('?')[0]}`, order.ok, order.observed);
    }
    // Body-named organization (SSO) is bound to the principal: foreign and
    // missing organizations are the same typed 404 even though SSO is off.
    for (const [route, makeBody] of [
      ['/native/sso/start', (org) => ({ organization: org, redirect_uri: 'http://127.0.0.1/callback' })],
      ['/native/sso/callback', (org) => ({
        organization: org,
        redirect_uri: 'http://127.0.0.1/callback',
        code: 'c',
        state: 's',
      })],
    ]) {
      const foreign = await request(base, null, 'POST', route, makeBody(orgA.organization.id), 2000, hdr(orgB.token));
      const missing = await request(base, null, 'POST', route, makeBody(missingOrg), 2000, hdr(orgB.token));
      const order = assertForeignOrgOrder(foreign, missing, [orgA.organization.id, orgA.owner_email]);
      add(`foreign-org:${route}`, order.ok, order.observed);
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
      disabled_subsystem_order_cells: disabledSubsystemOrderCells,
      invalid_control_refusals: invalidControlRefusals,
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
