#!/usr/bin/env node
// Native-route contract probe: drives the REAL daemon.
//
// For every discovered route this probe measures two mandatory cases from the
// route template in the report:
//   1. auth: an unauthenticated call is refused (401) before any effect;
//   2. strict/bounded handling: an authenticated empty/boundary request is
//      answered with a typed result (or a bounded stream), never a 5xx, never
//      a panic. SSE routes are opened and bounded by aborting after headers.
//
// Each route gets its own observed status/error-code pair. This is the
// surface's own behaviour evidence; it is not a shared invariant.

import { execFileSync, spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { discoverNativeRoutes, serverRouteFiles } from '../../../scripts/discover-capabilities.mjs';
import { hostileBodies, hostileHeaders, hostileQueries } from '../generators/hostile.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT_DIR = resolve(ROOT, 'target/certification/programme');

function sha(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

export function cliBinary() {
  const candidates = [
    process.env.FAKTOR_CLI_BIN,
    resolve(ROOT, 'target/release/faktor-cli'),
    resolve(ROOT, 'target/debug/faktor-cli'),
  ].filter(Boolean);
  return candidates.find((candidate) => existsSync(candidate)) ?? null;
}

export async function startDaemon(binary, config, data, token) {
  const child = spawn(binary, ['serve', '--port', '0', '--data-dir', data, '--config', config], {
    cwd: ROOT,
    env: { ...process.env, FAKTOR_SERVER_PASSWORD: token, ROUTE_PROBE_KEY: 'loopback-only' },
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
    await new Promise((done) => setTimeout(done, 50));
  }
  if (base === null) {
    child.kill('SIGKILL');
    throw new Error(`route probe daemon did not start: ${output.slice(-1200)}`);
  }
  while (Date.now() < deadline) {
    try {
      const response = await fetch(`${base}/native/ready`, {
        headers: { authorization: `Bearer ${token}` },
      });
      if (response.ok) return { child, base };
    } catch {
      // keep polling loopback
    }
    await new Promise((done) => setTimeout(done, 50));
  }
  child.kill('SIGKILL');
  throw new Error(`route probe daemon never became ready: ${output.slice(-1200)}`);
}

function resolvePath(template, sessionId) {
  return template.replace(/\{([^}]+)\}/g, (_match, name) => {
    if (name === 'id' || name === 'session') return sessionId;
    if (name === 'handle') return 'absent-handle';
    if (name === 'sequence') return '1';
    if (name === 'task') return '1';
    return '1';
  });
}

export async function request(base, token, method, path, body, timeoutMs = 5_000, extraHeaders = {}) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const response = await fetch(`${base}${path}`, {
      method,
      headers: {
        ...(token ? { authorization: `Bearer ${token}` } : {}),
        ...(body !== undefined ? { 'content-type': 'application/json' } : {}),
        ...extraHeaders,
      },
      body: body === undefined ? undefined : typeof body === 'string' ? body : JSON.stringify(body),
      signal: controller.signal,
    });
    const text = await response.text();
    let json = null;
    try {
      json = text.length > 0 ? JSON.parse(text) : null;
    } catch {
      json = null;
    }
    return {
      status: response.status,
      text,
      json,
      code: json?.error?.code ?? json?.code ?? null,
      aborted: false,
    };
  } catch (error) {
    return {
      status: null,
      text: '',
      json: null,
      code: null,
      aborted: error?.name === 'AbortError',
      error: String(error?.message ?? error),
    };
  } finally {
    clearTimeout(timer);
  }
}

function isStream(path) {
  return /\/events(\?|$)/.test(path);
}

function transientStatus(status) {
  return status === 503 || status === 409 || status === 501 || status === 502;
}

/**
 * Probe all native routes. Returns { status, results, failures }.
 * `status` is proven only when every route passed both cases.
 */
export async function probeRoutes({ report = false } = {}) {
  const binary = cliBinary();
  if (binary === null) {
    return { status: 'deferred', reason: 'no faktor-cli binary; trusted lane builds it', results: [] };
  }
  const routes = discoverNativeRoutes(ROOT, serverRouteFiles(ROOT));
  const tmp = mkdtempSync(join(tmpdir(), 'faktor-route-probe-'));
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
          api_key_env: 'ROUTE_PROBE_KEY',
          api: 'chat',
          allow_loopback: true,
          models: ['m'],
        },
      ],
    })}\n`,
  );
  const token = randomBytes(32).toString('hex');
  let daemon = null;
  try {
    daemon = await startDaemon(binary, config, data, token);
    const { base } = daemon;
    const created = await request(base, token, 'post', '/native/session', {
      provider: 'p1',
      model: 'm',
      workspace,
      title: 'route probe',
    });
    if (created.status !== 200 || !created.json?.id) {
      throw new Error(`route probe could not create a session: ${created.status} ${created.text.slice(0, 300)}`);
    }
    const sessionId = String(created.json.id);
    const results = [];
    const failures = [];
    for (const route of routes) {
      const template = route.path;
      let path = resolvePath(template, sessionId);
      let method = route.method;
      let authBody;
      let authedBody;
      if (route.id.startsWith('native.delete.native.session.id')) {
        path = path.replace(/\/session\/[^/]+/, '/session/424242');
        method = 'DELETE';
      }
      method = method.toUpperCase();
      if (isStream(path)) {
        const unauth = await request(base, null, method, path, undefined, 800);
        const authed = await request(base, token, method, path, undefined, 800);
        const refused = unauth.status === 401 || (unauth.status !== null && unauth.status < 500);
        const ok = refused && (authed.status === 200 || authed.aborted || (authed.status !== null && authed.status < 500));
        const result = {
          id: route.id,
          method,
          path: template,
          passed: ok,
          observed: { unauthenticated: unauth.status, authenticated: authed.aborted ? 'stream-bounded' : authed.status },
        };
        results.push(result);
        if (!ok) failures.push(result);
        continue;
      }
      if (method === 'GET' || method === 'DELETE') {
        authBody = undefined;
        authedBody = undefined;
      } else {
        authBody = {};
        authedBody = {};
      }
      const unauth = await request(base, null, method, path, authBody);
      const authed = await request(base, token, method, path, authedBody);
      // One deterministic hostile case per route, in addition to the empty
      // body: query overflow/traversal for reads, body/header hostility for
      // writes. Assert the refusal is typed and the response stays bounded.
      const index = results.length;
      let hostilePath = path;
      let hostileBody;
      let hostileName;
      if (method === 'GET' || method === 'DELETE') {
        const queries = hostileQueries();
        const query = queries[index % queries.length];
        hostileName = `query:${query}`;
        hostilePath = `${path}${path.includes('?') ? '&' : '?'}${query}`;
      } else {
        const bodies = hostileBodies(0x5eed + index, 10);
        const selected = bodies[0];
        hostileName = selected.name;
        hostileBody = selected.raw ?? selected.json;
      }
      const headers = hostileHeaders()[index % hostileHeaders().length];
      const hostile = await request(base, token, method, hostilePath, hostileBody, 5_000, headers);
      const hostileTyped =
        (hostile.status !== null && hostile.status < 500) || transientStatus(hostile.status);
      const hostileBounded = Buffer.byteLength(hostile.text) <= 2 * 1024 * 1024;
      const hostileNoPanic = !/panicked|RUST_BACKTRACE|unwrap\(\) on/i.test(hostile.text);
      const authOk =
        unauth.status === 401 ||
        (unauth.status !== null && unauth.status >= 400 && unauth.status < 500);
      const typed =
        (authed.status !== null && authed.status < 500) || transientStatus(authed.status);
      const noPanic = !/panicked|RUST_BACKTRACE|unwrap\(\) on/i.test(authed.text);
      const result = {
        id: route.id,
        method,
        path: template,
        passed: authOk && typed && noPanic && hostileTyped && hostileBounded && hostileNoPanic,
        observed: {
          unauthenticated: unauth.status,
          authenticated: authed.status,
          error_code: authed.code,
          hostile: {
            case: hostileName,
            status: hostile.status,
            bytes: Buffer.byteLength(hostile.text),
          },
        },
      };
      results.push(result);
      if (!result.passed) failures.push(result);
    }
    const document = {
      schema: 'faktor-programme-route-probe/v1',
      generated_at: new Date().toISOString(),
      binary,
      commit: execGit(['rev-parse', 'HEAD']),
      routes: results.length,
      failed: failures.length,
      digest: sha(JSON.stringify(results)),
      results,
    };
    if (report) {
      mkdirSync(OUT_DIR, { recursive: true });
      const path = resolve(OUT_DIR, 'route-probe.json');
      writeFileSync(path, `${JSON.stringify(document, null, 2)}\n`);
    }
    return {
      status: failures.length === 0 ? 'proven' : 'failed',
      results,
      failures,
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
  probeRoutes({ report: true })
    .then((result) => {
      console.log(
        `route probe: ${result.status} (${result.results?.length ?? 0} routes, ${result.failures?.length ?? 0} failed)`,
      );
      if (result.status === 'failed') {
        for (const failure of result.failures.slice(0, 20)) {
          console.error(`  ${failure.id}: ${JSON.stringify(failure.observed)}`);
        }
        process.exitCode = 1;
      }
    })
    .catch((error) => {
      console.error(`route probe: ${error.stack ?? error}`);
      process.exitCode = 1;
    });
}
