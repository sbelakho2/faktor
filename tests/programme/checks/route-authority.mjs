#!/usr/bin/env node
// Static route-authority check: every daemon route declares its principal
// class and scope in `route-authority.json`, and the registered handler
// actually ENFORCES that declaration.
//
// This closes the "new route is invisible" hole: a route discovered in the
// server source with no authority row fails, a row with no discovered route
// fails, and a handler whose source no longer contains the required gate
// (daemon password, control-plane principal, worker/registration token,
// webhook signature, session/child resolution) fails loudly.
//
// The check is source-derived (no daemon needed) and is called by both the
// route probe and the auth/tenant matrix probe before they measure anything.

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  discoverNativeRoutes,
  normalizeCapabilityId,
  serverRouteFiles,
} from '../../../scripts/discover-capabilities.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
export const AUTHORITY_PATH = 'tests/programme/route-authority.json';

const PRINCIPALS = new Set([
  'daemon',
  'daemon_bootstrap',
  'daemon_control',
  'daemon_control_token',
  'daemon_or_control_org',
  'worker_token',
  'webhook',
  'public',
]);
const SCOPES = new Set([
  'global',
  'session',
  'session_path',
  'session_query',
  'session_child',
  'session_resolver',
  'organization',
  'worker',
  'mixed_org',
  'resource',
]);
const ROLES = new Set(['bootstrap', 'any', 'viewer', 'member', 'admin', 'owner', 'worker']);

// Admin-only surfaces the report names explicitly. A declaration that
// weakens one of these (or a route that disappears) fails the scan.
const REQUIRED_MIN_ROLE = Object.freeze({
  'native.post.native.credits.grant': 'admin',
  'native.get.native.enterprise.audit': 'viewer',
  'native.post.native.enterprise.retention.gc': 'admin',
  'native.put.native.enterprise.settings': 'admin',
  'native.post.native.enterprise.deletion-jobs': 'owner',
  'native.post.native.enterprise.deletion-jobs.id.advance': 'owner',
  'native.post.native.updater.apply': 'admin',
  'native.post.native.updater.rollback': 'admin',
  'native.post.native.updater.downgrade': 'admin',
  'native.post.native.workers.tokens': 'admin',
  'native.post.native.workers.id.revoke': 'admin',
});
const EXACT_CREDENTIAL_ROUTES = Object.freeze({
  webhook: ['native.post.native.scm.webhook'],
  worker_token: [
    'native.post.native.jobs.claim',
    'native.post.native.jobs.id.result',
    'native.post.native.workers.id.heartbeat',
    'native.post.native.workers.register',
  ],
  daemon_bootstrap: ['native.post.native.orgs'],
  public: [],
});

function read(root, rel) {
  const path = join(root, rel);
  return existsSync(path) ? readFileSync(path, 'utf8') : '';
}

function walkRust(root) {
  const out = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) walk(path);
      else if (entry.name.endsWith('.rs') && !entry.name.endsWith('_tests.rs')) out.push(path);
    }
  };
  walk(join(root, 'crates/server/src'));
  return out;
}

/** Balanced body between the pair opened at `openIndex`, skipping strings/chars. */
function balancedBody(text, openIndex, open, close) {
  let depth = 0;
  let quote = null;
  for (let i = openIndex; i < text.length; i += 1) {
    const ch = text[i];
    if (quote) {
      if (ch === '\\') i += 1;
      else if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"') {
      quote = ch;
      continue;
    }
    if (ch === "'") {
      const literal = /^'(?:\\.|[^'\\])'/.exec(text.slice(i, i + 8));
      if (literal) i += literal[0].length - 1;
      continue;
    }
    if (ch === open) depth += 1;
    else if (ch === close) {
      depth -= 1;
      if (depth === 0) return text.slice(openIndex + 1, i);
    }
  }
  return null;
}

/** Every `fn name` body in the server tree, keyed by name. */
function functionBodies(root) {
  const map = new Map();
  for (const file of walkRust(root)) {
    const text = readFileSync(file, 'utf8');
    for (const match of text.matchAll(/(?:^|\n)\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([a-z_0-9]+)\s*[<(]/g)) {
      const open = text.indexOf('{', match.index + match[0].length);
      if (open < 0) continue;
      const body = balancedBody(text, open, '{', '}');
      if (body === null) continue;
      const entry = { file: relative(root, file), source: `${match[0]}\n${body}` };
      if (!map.has(match[1])) map.set(match[1], []);
      map.get(match[1]).push(entry);
    }
  }
  return map;
}

/** Route id -> { method, path, file, handler }. */
function discoveredHandlers(root) {
  const out = new Map();
  for (const file of serverRouteFiles(root)) {
    const text = read(root, file);
    for (const match of text.matchAll(/\.route\s*\(/g)) {
      const open = match.index + match[0].length - 1;
      const body = balancedBody(text, open, '(', ')');
      if (body === null) continue;
      const pathMatch = /^\s*"([^"]+)"/.exec(body);
      if (!pathMatch) continue;
      const rawPath = pathMatch[1];
      const path = rawPath.replace(/^\//, '').replace(/[/{}]/g, '.').replace(/\.+/g, '.');
      const chain = body.replace(pathMatch[0], '');
      for (const method of chain.matchAll(/\b(get|post|put|patch|delete)\s*\(\s*([A-Za-z0-9_:]+)/g)) {
        const id = normalizeCapabilityId(`native.${method[1]}.${path.replace(/\.$/, '')}`);
        const handler = method[2].split('::').pop();
        out.set(id, { method: method[1], path: rawPath, file: relative(root, join(root, file)), handler });
      }
    }
  }
  return out;
}

function handlerSource(root, functions, route) {
  const local = functions.get(route.handler) ?? [];
  const inFile = local.find((entry) => entry.file === route.file) ?? local[0];
  if (inFile) return inFile;
  const all = walkRust(root).flatMap((file) => {
    const text = readFileSync(file, 'utf8');
    const match = new RegExp(`fn\\s+${route.handler}\\s*[<(]`).exec(text);
    if (!match) return [];
    const open = text.indexOf('{', match.index + match[0].length);
    const body = open < 0 ? null : balancedBody(text, open, '{', '}');
    return body === null ? [] : [{ file: relative(root, file), source: body }];
  });
  return all[0] ?? null;
}

/** Source reachable from a handler through one- and two-hop helper calls. */
function reachableSource(root, functions, source, depth = 2, seen = new Set()) {
  let combined = source;
  if (depth === 0) return combined;
  for (const call of source.matchAll(/\b([a-z_0-9]+)\s*\(/g)) {
    const name = call[1];
    if (seen.has(name)) continue;
    seen.add(name);
    const bodies = functions.get(name);
    if (!bodies) continue;
    for (const entry of bodies.slice(0, 4)) {
      combined += `\n// reachable: ${name} (${entry.file})\n${reachableSource(root, functions, entry.source, depth - 1, seen)}`;
    }
  }
  return combined;
}

function hasAny(source, patterns) {
  return patterns.some((pattern) => source.includes(pattern));
}

const GATE = {
  daemon: ['authed('],
  daemon_bootstrap: ['authed(', 'resolve_principal('],
  daemon_control: ['authed(', ['require_principal(', 'updater_gate(']],
  daemon_control_token: ['authed(', ['control_token(', 'require_principal(']],
  daemon_or_control_org: ['authed(', ['require_principal(', 'resolve_org(', 'authorize(']],
  worker_token: [['worker_token(', 'token_organization(', 'worker_for_token(', 'plane.']],
  webhook: [['sink.deliver(', 'WebhookHeaders', 'deliver(']],
  public: [],
};

const SCOPE_GATE = {
  session_path: ['native_resolve_session(', 'resolve_handle(', 'get_session('],
  session_query: ['native_resolve_session(', 'resolve_handle(', 'get_session('],
  session_resolver: ['permissions.resolve('],
  session_child: [
    'control_child(',
    'durable_child_row(',
    'stage_child_changes(',
    'approve_and_merge(',
    'orchestrator.child(',
  ],
  organization: [],
  mixed_org: [],
  global: [],
  session: ['parse_session_id('],
  worker: [],
  resource: [],
};

function gateMatches(source, requirement) {
  if (typeof requirement === 'string') return source.includes(requirement);
  return hasAny(source, requirement);
}

/**
 * Run the static check. `observed.errors` is an array of human-readable
 * failures; `passed` is true only when it is empty.
 */
export function scan({ root = ROOT } = {}) {
  const authorityPath = join(root, AUTHORITY_PATH);
  const errors = [];
  let document;
  try {
    document = JSON.parse(readFileSync(authorityPath, 'utf8'));
  } catch (error) {
    return {
      passed: false,
      observed: { routes: 0, declared: 0, errors: [`cannot read ${AUTHORITY_PATH}: ${error.message}`] },
    };
  }
  const entries = Array.isArray(document.entries) ? document.entries : [];
  const byId = new Map();
  for (const entry of entries) {
    if (!entry || typeof entry.id !== 'string') {
      errors.push('authority row without an id');
      continue;
    }
    if (byId.has(entry.id)) errors.push(`${entry.id}: duplicate authority row`);
    if (!PRINCIPALS.has(entry.principal)) {
      errors.push(`${entry.id}: unknown principal class ${JSON.stringify(entry.principal)}`);
    }
    if (!SCOPES.has(entry.scope)) {
      errors.push(`${entry.id}: unknown scope ${JSON.stringify(entry.scope)}`);
    }
    if (entry.min_role !== undefined && !ROLES.has(entry.min_role)) {
      errors.push(`${entry.id}: unknown min_role ${JSON.stringify(entry.min_role)}`);
    }
    byId.set(entry.id, entry);
  }

  const routes = discoveredHandlers(root);
  const functions = functionBodies(root);
  for (const id of routes.keys()) {
    if (!byId.has(id)) errors.push(`${id}: discovered route has NO authority declaration`);
  }
  for (const id of byId.keys()) {
    if (!routes.has(id)) errors.push(`${id}: authority declaration has NO discovered route`);
  }

  for (const [id, route] of routes) {
    const entry = byId.get(id);
    if (!entry) continue;
    const found = handlerSource(root, functions, route);
    if (!found) {
      errors.push(`${id}: handler ${route.handler} not found in the server source`);
      continue;
    }
    const source = reachableSource(root, functions, found.source);
    const requirements = GATE[entry.principal] ?? [];
    for (const requirement of requirements) {
      if (!gateMatches(source, requirement)) {
        const label = Array.isArray(requirement) ? requirement.join(' | ') : requirement;
        errors.push(`${id}: ${entry.principal} handler ${route.handler} is missing ${label}`);
      }
    }
    if ((entry.principal === 'worker_token' || entry.principal === 'webhook') && source.includes('authed(')) {
      errors.push(`${id}: ${entry.principal} handler must not accept the daemon password (authed found)`);
    }
    const scopeRequirements = SCOPE_GATE[entry.scope] ?? [];
    if (scopeRequirements.length > 0 && !hasAny(source, scopeRequirements)) {
      errors.push(
        `${id}: ${entry.scope} handler ${route.handler} is missing one of ${scopeRequirements.join(' | ')}`,
      );
    }
  }

  for (const [id, expected] of Object.entries(REQUIRED_MIN_ROLE)) {
    const entry = byId.get(id);
    if (!entry) {
      errors.push(`${id}: required admin-only route is missing from the authority table`);
    } else if (entry.min_role !== expected) {
      errors.push(`${id}: admin-only route declares min_role ${entry.min_role}, expected ${expected}`);
    }
  }
  for (const [principal, expected] of Object.entries(EXACT_CREDENTIAL_ROUTES)) {
    const actual = entries.filter((entry) => entry.principal === principal).map((entry) => entry.id).sort();
    const wanted = [...expected].sort();
    if (JSON.stringify(actual) !== JSON.stringify(wanted)) {
      errors.push(`${principal}: credential routes changed; expected ${wanted.join(', ')}, got ${actual.join(', ')}`);
    }
  }

  const counts = {};
  for (const entry of entries) {
    counts[entry.principal] = (counts[entry.principal] ?? 0) + 1;
    if (entry.scope === 'session_path' && !/\{(?:id|session)\}/.test(entry.path)) {
      errors.push(`${entry.id}: session_path scope requires a path session parameter`);
    }
    if (entry.scope === 'organization' && entry.principal !== 'daemon_control' && entry.principal !== 'daemon_control_token' && entry.principal !== 'daemon_bootstrap') {
      errors.push(`${entry.id}: organization scope requires a control-plane principal`);
    }
  }
  const scopes = {};
  for (const entry of entries) {
    scopes[entry.scope] = (scopes[entry.scope] ?? 0) + 1;
  }
  return {
    passed: errors.length === 0,
    observed: {
      routes: routes.size,
      declared: entries.length,
      principals: counts,
      scopes,
      errors,
    },
  };
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const result = scan();
  console.log(
    `route authority: ${result.passed ? 'PASS' : 'FAIL'} (${result.observed.declared}/${result.observed.routes} routes declared)`,
  );
  for (const error of result.observed.errors.slice(0, 40)) console.error(`  ${error}`);
  if (!result.passed) process.exitCode = 1;
}
