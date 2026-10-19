#!/usr/bin/env node
// THE shared authorization-ordering rule for BOTH auth matrices.
//
// RULE: unless a surface is explicitly public/bootstrap, an invalid or
// foreign principal must lose BEFORE any feature-state disclosure. No
// `*_disabled`, plan, tenant or existence oracle may answer a caller who has
// not presented a valid principal. The two matrices (the independent
// verification probe and the generated programme probe) import this module
// so they enforce exactly the same ordering, not two drifting copies.
//
// Explicit exceptions (documented at the route):
//   * `/native/scm/webhook` is the one unauthenticated native surface: the
//     HMAC signature is its credential and a sink must exist to verify it, so
//     `scm_webhook_disabled` is its configuration refusal (never a product
//     read).
//   * `POST /native/orgs` is the daemon-password bootstrap path: with NO
//     control-plane token presented it answers `cloud_disabled`; presenting
//     an unverifiable token is still an invalid principal (401).

/** Typed feature/subsystem state codes that must never precede auth. */
export const FEATURE_STATE_CODES = new Set([
  'cloud_disabled',
  'scm_disabled',
  'scm_webhook_disabled',
  'billing_disabled',
  'updater_disabled',
  'workers_disabled',
  'enterprise_disabled',
  'sso_disabled',
  'sso_not_configured',
  'foundation_disabled',
  'retention_runtime_disabled',
  'plan_not_found',
  'value_entitlement_refused',
  'subscription_inactive',
]);

/** Public surfaces whose credential cannot be resolved without the feature. */
export const PUBLIC_ROUTES = new Set(['/native/scm/webhook']);
/** Bootstrap surface that accepts the daemon password with no token. */
export const BOOTSTRAP_ROUTES = new Set(['/native/orgs']);

/** Route templates whose credential is a worker registration/session token. */
export const WORKER_CREDENTIAL_ROUTES = new Set([
  '/native/workers/register',
  '/native/workers/{id}/heartbeat',
  '/native/jobs/claim',
  '/native/jobs/{id}/result',
]);

export const WORKER_PROTOCOL_VERSION = 1;

/** Principal classes that must present `x-faktor-control-token`. */
export function isControlPrincipal(principal) {
  return (
    principal === 'daemon_control' ||
    principal === 'daemon_control_token' ||
    principal === 'daemon_or_control_org' ||
    principal === 'daemon_bootstrap'
  );
}

/**
 * Routes that REQUIRE the control token on every branch (unlike the
 * `daemon_or_control_org` dual surface, which is admitted by the daemon
 * password alone on its local branch).
 */
export function requiresControlToken(principal) {
  return principal === 'daemon_control' || principal === 'daemon_control_token';
}

/**
 * A well-formed body carrying an UNBOUND worker credential. Both matrices
 * send exactly this so a disabled worker plane cannot be told apart from an
 * unknown token.
 */
export function credentialProbeBody(path) {
  if (path === '/native/workers/register') {
    return {
      token: 'wkr_auth_order_probe',
      worker_id: 'wrk_probe',
      display_name: 'probe',
      capabilities: {
        os: 'linux',
        arch: 'x86_64',
        toolchains: ['rust'],
        sandbox: [],
        network: 'none',
        cpu_cores: 1,
        memory_mb: 1024,
        gpu: null,
        region: 'matrix',
        trust_domain: 'matrix',
        protocol_version: WORKER_PROTOCOL_VERSION,
      },
    };
  }
  if (path === '/native/workers/{id}/heartbeat') {
    return {
      token: 'wkr_auth_order_probe',
      protocol_version: WORKER_PROTOCOL_VERSION,
      lease_id: 'lease_probe',
      generation: 1,
    };
  }
  if (path === '/native/jobs/claim') {
    return { token: 'wkr_auth_order_probe', protocol_version: WORKER_PROTOCOL_VERSION };
  }
  if (path === '/native/jobs/{id}/result') {
    return {
      token: 'wkr_auth_order_probe',
      protocol_version: WORKER_PROTOCOL_VERSION,
      generation: 1,
      lease_id: 'lease_probe',
      digest: 'a'.repeat(64),
      outcome: 'succeeded',
    };
  }
  return null;
}

export function errorCode(result) {
  return result?.json?.error?.code ?? result?.json?.code ?? null;
}

export function typedRefusal(result) {
  return (
    result?.status !== null &&
    result?.status >= 400 &&
    result?.status < 500 &&
    errorCode(result) !== null &&
    Buffer.byteLength(result.text ?? '') <= 4096
  );
}

export function featureStateDisclosed(result) {
  const code = errorCode(result);
  if (code !== null && FEATURE_STATE_CODES.has(code)) return true;
  return /(?:^|_)(?:disabled|not_configured)$/.test(code ?? '') ||
    /^(?:plan|subscription|entitlement)_/.test(code ?? '');
}

/**
 * The one rule: an invalid/foreign principal is refused with a typed 401 and
 * nothing else. Returns `{ ok, observed }` so both matrices record the same
 * evidence shape.
 */
export function assertAuthOrder(result) {
  const code = errorCode(result);
  const ok =
    result?.status === 401 &&
    code !== null &&
    typedRefusal(result) &&
    !featureStateDisclosed(result);
  return { ok, observed: { status: result?.status ?? null, code } };
}

/**
 * A foreign org is the SAME typed 404 a missing org answers (no existence or
 * feature-state oracle). `missing` is the response for an id that never
 * existed.
 */
export function assertForeignOrgOrder(foreign, missing, forbidden = []) {
  const code = errorCode(foreign);
  const missingCode = errorCode(missing);
  const leaks = forbidden.filter((value) => value && foreign?.text?.includes(value));
  const ok =
    foreign?.status === 404 &&
    missing?.status === 404 &&
    code !== null &&
    code === missingCode &&
    typedRefusal(foreign) &&
    !featureStateDisclosed(foreign) &&
    leaks.length === 0;
  return {
    ok,
    observed: {
      status: foreign?.status ?? null,
      code,
      missing_status: missing?.status ?? null,
      missing_code: missingCode,
      leaks,
    },
  };
}

/** The explicit public/bootstrap exceptions, kept in one place. */
export function isOrderingException(method, path) {
  if (PUBLIC_ROUTES.has(path)) return 'public';
  if (BOOTSTRAP_ROUTES.has(path) && method.toUpperCase() === 'POST') return 'bootstrap';
  return null;
}
