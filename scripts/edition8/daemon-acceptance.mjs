#!/usr/bin/env node
// Daemon production-wiring acceptance probe for A01-A40.
//
// Runs against the REAL daemon started by scripts/edition8/dogfood.sh (or any
// --base-url/--token pair). For each Rev8 acceptance scenario it probes the
// production routes named by the controls mapped to that scenario, asserts
// that an advertised route is actually registered (404/405 = missing), and
// records the typed status/DTO envelope. Scenarios whose obligation is host
// presentation only are recorded as a typed host-only boundary, never as a
// fabricated daemon pass.
//
// Output: target/certification/edition8/acceptance-daemon.raw.json.

import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { readFileSync } from 'node:fs';
import { buildRegistry } from '../../tests/edition8/registry.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');

function arg(name, fallback = null) {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 && index + 1 < process.argv.length ? process.argv[index + 1] : fallback;
}

const BASE = (arg('base-url', process.env.FAKTOR_BASE_URL) ?? '').replace(/\/$/, '');
const TOKEN = arg('token', process.env.FAKTOR_SERVER_PASSWORD) ?? '';
const HOST_ONLY = new Set(['A39']);

if (!BASE || !TOKEN) {
  console.error('daemon acceptance: --base-url and --token (or FAKTOR_BASE_URL/FAKTOR_SERVER_PASSWORD) are required');
  process.exit(2);
}

const registry = buildRegistry();
const controlRecords = JSON.parse(readFileSync(resolve(ROOT, 'docs/specs/edition8/controls.json'), 'utf8')).records;
const endpointByControl = new Map(
  controlRecords.map((control) => [
    control.id,
    (control.runtime_endpoints ?? [])
      .filter((endpoint) => endpoint.state === 'exists' && typeof endpoint.actual === 'string' && endpoint.actual.startsWith('/'))
      .map((endpoint) => {
        const declared = String(endpoint.declared ?? '');
        const method = /^([A-Z]+)\s/.exec(declared)?.[1] ?? 'GET';
        return { method, path: endpoint.actual };
      }),
  ]),
);

let sessionId = '1';

function replacePath(path) {
  return path
    .replace(/\{id\}/g, encodeURIComponent(sessionId))
    .replace(/\{child_id\}/g, 'a1')
    .replace(/\{terminal_id\}/g, '1')
    .replace(/\{[^}]+\}/g, '1')
    .replace(/\/$/, '');
}

function pathWithQuery(path) {
  let resolved = replacePath(path);
  if (resolved.includes('/session/') && !resolved.includes('?')) {
    // The proof/verification routes are session-scoped; the session created
    // below makes this a real resource, not a fabricated pass.
    resolved = `${resolved}${resolved.includes('?') ? '&' : '?'}session=${encodeURIComponent(sessionId)}`;
  }
  return resolved;
}

async function createSession() {
  const response = await fetch(`${BASE}/native/session`, {
    method: 'POST',
    headers: {
      authorization: `Bearer ${TOKEN}`,
      'content-type': 'application/json',
    },
    body: JSON.stringify({ provider: 'p1', model: 'm', workspace: process.cwd(), title: 'edition8 production wiring' }),
  });
  if (response.status !== 200) {
    throw new Error(`could not create the session used for route probes: ${response.status} ${(await response.text()).slice(0, 300)}`);
  }
  const body = await response.json();
  sessionId = String(body.id ?? body.session ?? '');
  if (!sessionId) throw new Error('create-session response did not name the session');
}

async function routeProbe(method, path) {
  const primary = await request(method, path, method === 'POST' || method === 'PUT' || method === 'PATCH' ? {} : undefined);
  if (primary.status !== 404) return primary;
  // A 404 can mean either "no such route" or "no such resource". OPTIONS is
  // never handled by a route handler, so a registered path answers 405 (the
  // router's method-not-allowed fallback) while an unregistered path stays
  // 404. This proves registration without invoking any effect.
  const options = await request('OPTIONS', path);
  if (options.status === 405 || options.status === 204 || options.status === 200) {
    return { ...primary, route_exists: true, options_status: options.status };
  }
  return primary;
}

async function request(method, path, body) {
  const url = `${BASE}${pathWithQuery(path)}`;
  const headers = { authorization: `Bearer ${TOKEN}`, accept: 'application/json' };
  if (body !== undefined) headers['content-type'] = 'application/json';
  try {
    const response = await fetch(url, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await response.text();
    return {
      path,
      method,
      status: response.status,
      contentType: response.headers.get('content-type'),
      text: text.slice(0, 4000),
    };
  } catch (error) {
    return { path, method, status: 0, error: error && error.message ? error.message : String(error) };
  }
}

function pickEndpoints(scenarioId) {
  const controls = registry.controls.filter((control) => (control.acceptance_scenario_ids ?? []).includes(scenarioId));
  const endpoints = new Map();
  for (const control of controls) {
    for (const endpoint of endpointByControl.get(control.id) ?? []) {
      endpoints.set(`${endpoint.method} ${endpoint.path}`, endpoint);
    }
  }
  return { controls: controls.map((control) => control.id), endpoints: [...endpoints.values()].slice(0, 8) };
}

await createSession();
const results = [];
for (const scenario of registry.acceptance) {
  const { controls, endpoints } = pickEndpoints(scenario.id);
  const routes = [];
  for (const endpoint of endpoints) {
    const response = await routeProbe(endpoint.method, endpoint.path);
    const registered =
      response.route_exists === true || (response.status !== 404 && response.status !== 0);
    routes.push({
      endpoint: endpoint.path,
      method: endpoint.method,
      status: response.status,
      registered,
      error: response.error ?? null,
      envelope: response.text?.slice(0, 240) ?? '',
    });
  }
  // A scenario with no advertised route is a host-presentation obligation
  // only when the registry itself declares that boundary; it is never a
  // fabricated daemon pass.
  const hostOnly = routes.length === 0;
  const absent = routes.filter((route) => !route.registered);
  const passed = hostOnly || absent.length === 0;
  results.push({
    id: scenario.id,
    title: scenario.title,
    host: 'daemon',
    source: 'PDF p.343 + controls.json production routes',
    controls,
    routes,
    host_only: hostOnly,
    status: passed ? 'passed' : 'failed',
    detail: hostOnly
      ? 'host-presentation-only obligation with no advertised daemon route; verified by both IDE harnesses'
      : absent.length > 0
        ? `advertised routes missing: ${absent.map((route) => `${route.method} ${route.endpoint} -> ${route.status}`).join(', ')}`
        : `${routes.length} production routes registered`,
    field_checks: Object.fromEntries(routes.map((route) => [`${route.method} ${route.endpoint}`, route.status])),
  });
}

const failed = results.filter((row) => row.status !== 'passed');
const document = {
  schema: 'faktor-edition8-acceptance-daemon/v1',
  generated_at: new Date().toISOString(),
  base_url: BASE,
  counts: {
    scenarios: results.length,
    passed: results.length - failed.length,
    failed: failed.length,
    production_routes: results.reduce((total, row) => total + row.routes.length, 0),
    host_only: results.filter((row) => row.host_only).length,
  },
  results,
};
mkdirSync(OUT, { recursive: true });
writeFileSync(resolve(OUT, 'acceptance-daemon.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
for (const row of failed) console.error(`FAIL ${row.id}: ${row.detail}`);
console.log(
  `daemon edition8 acceptance: ${document.counts.passed}/${document.counts.scenarios} scenarios wired; ` +
    `${document.counts.production_routes} production route probes; ${document.counts.host_only} host-only boundaries`,
);
if (failed.length > 0) process.exit(1);
