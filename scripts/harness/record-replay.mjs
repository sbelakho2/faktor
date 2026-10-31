#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=separate-process
//
// HAR-02 record/replay scrubber.
//
// Records a real provider exchange (request + response: headers and raw
// stream frames, byte-exact, with normalized relative timing), scrubs
// secrets DETERMINISTICALLY BEFORE anything touches disk, and replays the
// scrubbed fixture to a consumer (the loopback fakes or any HTTP client)
// byte-exactly: same frame boundaries, same frame order, timing shape kept
// as integer-ms deltas scaled by --timing-scale (default 0 = instant).
//
// Threat list the scrubber neutralizes (scrub by VALUE everywhere, not just
// by header name):
//   1. Authorization headers (Bearer tokens, basic auth)          -> «REDACTED:authorization»
//   2. api-key style headers (x-api-key, api-key, x-goog-api-key,
//      x-auth-token)                                              -> «REDACTED:<header>»
//   3. Bearer tokens appearing anywhere in raw bytes (regex)
//   4. sk-… style key material appearing anywhere in raw bytes (regex)
//   5. KEY-like env VALUES from --env-file (KEY=VALUE lines, value
//      length >= 8), scrubbed by VALUE wherever they appear
//      (headers, URLs, bodies, frames)                            -> «REDACTED:env:<KEY>»
//   6. URL userinfo passwords and key/token/secret query params
//   7. set-cookie header values                                   -> «REDACTED:cookie»
//   8. User-identifying home paths: /Users/<name>, /home/<name>,
//      C:\Users\<name>                                           -> «HOME»
// Every scrubbed value is also recorded as a sha256 prefix digest for
// audit; the value itself is never written.
//
// Modes:
//   node scripts/harness/record-replay.mjs record --url <url> --out <fixture.json>
//        [--method POST] [--body-json <json> | --body-file <path>]
//        [--header 'Name: value']... [--env-file <path>] [--allow-http]
//   node scripts/harness/record-replay.mjs replay --fixture <fixture.json>
//        [--port 0] [--port-file <path>] [--timing-scale <ms-per-ms>]
//   node scripts/harness/record-replay.mjs scrub --fixture <path> [--out <path>]
//   node scripts/harness/record-replay.mjs selftest
//
// Exit codes: 0 pass/served; 1 selftest/replay failure; 2 usage error.

import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const SCHEMA = 'faktor.harness.recording/v1';
const MAX_FRAME_MS = 60_000;
const HOME_PLACEHOLDER = '«HOME»';

const SECRET_HEADERS = new Set(['authorization', 'x-api-key', 'api-key', 'x-goog-api-key', 'x-auth-token', 'proxy-authorization']);
const COOKIE_HEADER = new Set(['set-cookie', 'cookie']);
const URL_SECRET_PARAM = /(?:^|[^a-z])(?:api[_-]?key|apikey|key|token|secret|password|authorization|access_token)=([^&\s"']+)/gi;
const BODY_JSON_SECRET_RE = /"(?:api[_-]?key|apikey|access_token|secret|token|password|authorization)"\s*:\s*"([^"\\]{8,})"/g;
const BEARER_RE = /Bearer\s+([A-Za-z0-9._~+/=-]{12,})/g;
const SK_RE = /\bsk-[A-Za-z0-9_-]{16,}\b/g;
const HOME_UNIX_RE = /\/(?:Users|home)\/([A-Za-z0-9._-]{1,64})/g;
const HOME_WIN_RE = /[A-Z]:\\Users\\([A-Za-z0-9._-]{1,64})/g;

function sha256hex(text) {
  return createHash('sha256').update(text, 'utf8').digest('hex');
}

/** Parse a KEY=VALUE env file read-only. Values shorter than 8 are ignored. */
export function parseEnvFile(text) {
  const out = [];
  for (const line of text.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const at = trimmed.indexOf('=');
    if (at <= 0) continue;
    const key = trimmed.slice(0, at).trim();
    const value = trimmed.slice(at + 1).trim();
    if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(key) && value.length >= 8) out.push({ key, value });
  }
  return out;
}

/**
 * Collect every secret VALUE present in a capture, with a deterministic
 * replacement label. Secrets are matched by value from headers, URL, body
 * and the env file — not by name alone.
 */
export function collectSecrets({ headers = [], url = '', body = '', envValues = [] } = {}) {
  const secrets = new Map(); // value -> label
  const add = (value, label) => {
    if (typeof value === 'string' && value.length >= 8 && !secrets.has(value)) secrets.set(value, label);
  };
  const headerValues = headers.map(([, value]) => value);
  for (const [name, value] of headers) {
    const lower = name.toLowerCase();
    if (SECRET_HEADERS.has(lower)) add(value, lower === 'authorization' ? 'authorization' : lower);
    if (COOKIE_HEADER.has(lower)) add(value, 'cookie');
  }
  // Pattern sweep over EVERY text channel: URL, header values, bodies and
  // (in scrubCapture) decoded frames — a `key=value` or bearer/sk- token
  // hides nowhere just because it sat in a header instead of the URL.
  const blob = [url, body, ...headerValues].join('\n');
  for (const match of blob.matchAll(BEARER_RE)) add(match[1], 'bearer');
  for (const match of blob.matchAll(SK_RE)) add(match[0], 'sk-key');
  for (const match of blob.matchAll(BODY_JSON_SECRET_RE)) add(match[1], 'body-field');
  for (const match of blob.matchAll(URL_SECRET_PARAM)) add(match[1], 'url-param');
  const userinfo = /^[a-z]+:\/\/[^/@]*:([^/@]+)@/i.exec(url);
  if (userinfo) add(userinfo[1], 'url-userinfo');
  for (const { key, value } of envValues) add(value, `env:${key}`);
  // Longest value first so a secret containing another is fully replaced.
  return [...secrets.entries()].sort((a, b) => b[0].length - a[0].length);
}

/** Replace every occurrence of each secret value; returns scrubbed text + applied labels. */
export function scrubText(text, secrets) {
  const applied = [];
  let out = text;
  for (const [value, label] of secrets) {
    if (!out.includes(value)) continue;
    applied.push(label);
    out = out.split(value).join(`«REDACTED:${label}»`);
  }
  return { text: out, applied };
}

/** Scrub user-identifying home paths (runs after secret values). */
export function scrubHomePaths(text) {
  let out = text.replace(HOME_WIN_RE, HOME_PLACEHOLDER);
  out = out.replace(HOME_UNIX_RE, HOME_PLACEHOLDER);
  return out;
}

/** Full deterministic scrub of a raw capture; nothing else changes. */
export function scrubCapture(raw, envValues) {
  const headers = [...raw.request.headers, ...raw.response.headers];
  const decodedFrames = raw.response.frames.map((f) => Buffer.from(f.data, 'base64').toString('utf8'));
  const secrets = collectSecrets({
    headers,
    url: raw.request.url,
    body: `${raw.request.body ?? ''}\n${decodedFrames.join('\n')}`,
    envValues,
  });
  const scrubHeaderList = (list) =>
    list.map(([name, value]) => {
      const { text } = scrubText(value, secrets);
      return [name, scrubHomePaths(text)];
    });
  const scrubBody = (text) => scrubHomePaths(scrubText(text, secrets).text);
  const frames = raw.response.frames.map((frame) => {
    const bytes = Buffer.from(frame.data, 'base64');
    const decoded = bytes.toString('utf8');
    if (!Buffer.from(decoded, 'utf8').equals(bytes)) {
      // Binary frame: the utf8 view is lossy, so text scrubbing could not be
      // byte-safe. Keep the original bytes and flag the frame loudly.
      return { data: frame.data, tMs: frame.tMs, binary: true };
    }
    const scrubbedText = scrubBody(decoded);
    if (scrubbedText === decoded) return { data: frame.data, tMs: frame.tMs };
    return { data: Buffer.from(scrubbedText, 'utf8').toString('base64'), tMs: frame.tMs };
  });
  return {
    schema: SCHEMA,
    meta: {
      ...raw.meta,
      scrubbed: true,
      frameCount: frames.length,
      totalBytes: frames.reduce((n, f) => n + Buffer.from(f.data, 'base64').length, 0),
    },
    request: {
      method: raw.request.method,
      url: scrubHomePaths(scrubText(raw.request.url, secrets).text),
      headers: scrubHeaderList(raw.request.headers),
      body: raw.request.body === undefined ? undefined : scrubBody(raw.request.body),
    },
    response: {
      status: raw.response.status,
      headers: scrubHeaderList(raw.response.headers),
      frames,
    },
    scrub: {
      secrets: secrets.length,
      digests: secrets.map(([value, label]) => ({
        label,
        sha256: `sha256:${sha256hex(value).slice(0, 16)}`,
      })),
    },
  };
}

/** Record one HTTP exchange into an in-memory raw capture (never written unscrubbed). */
async function recordExchange({ url, method, headers, body, allowHttp }) {
  const parsed = new URL(url);
  if (parsed.protocol === 'http:' && !(allowHttp && ['127.0.0.1', 'localhost', '::1'].includes(parsed.hostname))) {
    throw new Error('refusing to record plaintext http to a non-loopback host (pass --allow-http for loopback fixtures)');
  }
  const started = Date.now();
  const requestHeaderPairs = headers.map((line) => {
    const at = line.indexOf(':');
    return at === -1 ? [line, ''] : [line.slice(0, at).trim(), line.slice(at + 1).trim()];
  });
  const response = await fetch(url, { method, headers: requestHeaderPairs, body, redirect: 'manual' });
  const frameHeaders = [...response.headers.entries()];
  const contentType = response.headers.get('content-type') ?? '';
  const frames = [];
  let last = started;
  if (response.body) {
    const reader = response.body.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      const now = Date.now();
      frames.push({
        data: Buffer.from(value).toString('base64'),
        tMs: Math.min(MAX_FRAME_MS, Math.max(0, now - last)),
      });
      last = now;
    }
  }
  const requestHeaders = requestHeaderPairs;
  return {
    meta: {
      recordedAtEpochMs: started,
      contentShape: contentType.includes('event-stream') ? 'sse' : contentType.includes('ndjson') ? 'ndjson' : 'json',
    },
    request: { method, url, headers: requestHeaders, body },
    response: { status: response.status, headers: frameHeaders, frames },
  };
}

/** Serve a scrubbed fixture byte-exactly: one write per recorded frame. */
export function startReplayServer(fixture, { port = 0, timingScale = 0 } = {}) {
  const frames = fixture.response.frames.map((f) => Buffer.from(f.data, 'base64'));
  const server = createServer(async (request, response) => {
    const headers = {};
    for (const [name, value] of fixture.response.headers) {
      if (['content-length', 'transfer-encoding', 'connection', 'date'].includes(name.toLowerCase())) continue;
      headers[name] = value;
    }
    response.writeHead(fixture.response.status, headers);
    for (let i = 0; i < frames.length; i += 1) {
      if (timingScale > 0 && i > 0) {
        const delay = Math.min(MAX_FRAME_MS, Math.max(0, fixture.response.frames[i].tMs * timingScale));
        if (delay > 0) await new Promise((r) => setTimeout(r, delay));
      }
      // One write per recorded frame: frame boundaries are preserved exactly.
      if (!response.write(frames[i])) {
        await new Promise((r) => response.once('drain', r));
      }
    }
    response.end();
  });
  return new Promise((resolveServer) => {
    server.listen(port, '127.0.0.1', () => {
      resolveServer({ server, port: server.address().port, bytes: frames.reduce((n, b) => n + b.length, 0) });
    });
  });
}

/** Parse an SSE or NDJSON payload into a plain event-data sequence. */
export function parseEvents(payload, contentType = '') {
  if (contentType.includes('ndjson') || (!payload.includes('data:') && payload.includes('\n'))) {
    return payload
      .split('\n')
      .filter((line) => line.trim().length > 0)
      .map((line) => {
        try {
          return { json: JSON.parse(line) };
        } catch {
          return { text: line };
        }
      });
  }
  const events = [];
  for (const line of payload.split('\n')) {
    if (!line.startsWith('data:')) continue;
    const data = line.slice(5).trim();
    if (data === '[DONE]') {
      events.push({ done: true });
      continue;
    }
    try {
      events.push({ json: JSON.parse(data) });
    } catch {
      events.push({ text: data });
    }
  }
  return events;
}

async function main() {
  const args = process.argv.slice(2);
  const mode = args[0];
  const value = (name, fallback) => {
    const at = args.indexOf(`--${name}`);
    return at >= 0 && at + 1 < args.length ? args[at + 1] : fallback;
  };
  const values = (name) => {
    const out = [];
    for (let i = 0; i < args.length; i += 1) {
      if (args[i] === `--${name}` && i + 1 < args.length) out.push(args[i + 1]);
    }
    return out;
  };

  if (mode === 'record') {
    const url = value('url', null);
    const out = value('out', null);
    if (!url || !out) {
      console.error('record requires --url and --out');
      return 2;
    }
    const bodyJson = value('body-json', null);
    const bodyFile = value('body-file', null);
    const body = bodyJson ?? (bodyFile ? readFileSync(bodyFile, 'utf8') : undefined);
    const envFile = value('env-file', null);
    const envValues = envFile ? parseEnvFile(readFileSync(envFile, 'utf8')) : [];
    const raw = await recordExchange({
      url,
      method: value('method', body === undefined ? 'GET' : 'POST'),
      headers: [...values('header'), ...(body !== undefined ? ['content-type: application/json'] : [])],
      body,
      allowHttp: args.includes('--allow-http'),
    });
    // Scrub BEFORE the first byte touches disk.
    const fixture = scrubCapture(raw, envValues);
    const text = `${JSON.stringify(fixture, null, 2)}\n`;
    mkdirSync(dirname(resolve(out)), { recursive: true });
    writeFileSync(resolve(out), text);
    console.log(`record-replay: recorded ${fixture.response.frames.length} frames (${fixture.meta.totalBytes} bytes), scrubbed ${fixture.scrub.secrets} secrets -> ${out}`);
    return 0;
  }

  if (mode === 'replay') {
    const fixturePath = value('fixture', null);
    if (!fixturePath) {
      console.error('replay requires --fixture');
      return 2;
    }
    const fixture = JSON.parse(readFileSync(fixturePath, 'utf8'));
    if (fixture.schema !== SCHEMA) {
      console.error(`replay: fixture schema ${fixture.schema} != ${SCHEMA}`);
      return 1;
    }
    const { server, port, bytes } = await startReplayServer(fixture, {
      port: Number(value('port', '0')),
      timingScale: Number(value('timing-scale', '0')),
    });
    const portFile = value('port-file', null);
    if (portFile) writeFileSync(portFile, `${port}\n`);
    console.log(`record-replay: replaying ${fixture.response.frames.length} frames (${bytes} bytes) on http://127.0.0.1:${port}`);
    for (const signal of ['SIGTERM', 'SIGINT']) {
      process.on(signal, () => server.close(() => process.exit(0)));
    }
    // Keep serving until killed.
    return new Promise(() => {});
  }

  if (mode === 'scrub') {
    const fixturePath = value('fixture', null);
    if (!fixturePath) {
      console.error('scrub requires --fixture');
      return 2;
    }
    const raw = JSON.parse(readFileSync(fixturePath, 'utf8'));
    const envFile = value('env-file', null);
    const envValues = envFile ? parseEnvFile(readFileSync(envFile, 'utf8')) : [];
    const fixture = scrubCapture(raw, envValues);
    const text = `${JSON.stringify(fixture, null, 2)}\n`;
    const out = value('out', null) ?? fixturePath;
    writeFileSync(resolve(out), text);
    console.log(`record-replay: scrubbed ${fixture.scrub.secrets} secrets -> ${out}`);
    return 0;
  }

  if (mode === 'selftest') {
    return selftest();
  }

  console.error('usage: node scripts/harness/record-replay.mjs [record|replay|scrub|selftest] …');
  return 2;
}

// ------------------------------------------------------------------ selftest

/** Synthetic provider used only inside the selftest (loopback). */
function startSyntheticProvider({ echoSecretInFrame = false, homePathInFrame = false, extraResponseHeaders = {} } = {}) {
  const server = createServer((request, response) => {
    let body = '';
    request.on('data', (chunk) => {
      body += chunk;
    });
    request.on('end', () => {
      const marker = homePathInFrame ? `stored-at-${homedir()}/.faktor/session.json` : 'stored-at-fixture-path';
      const echo = echoSecretInFrame ? ' key=sk-planted-in-frame-0123456789abcdef' : '';
      const answer = `contract answer ${marker}${echo}`;
      response.writeHead(200, {
        'content-type': 'text/event-stream',
        'set-cookie': 'session=planted-cookie-secret-42aabb; Path=/',
        'x-request-echo': 'api_key=planted-query-secret-998877',
        ...extraResponseHeaders,
      });
      const frames = [
        `data: ${JSON.stringify({ choices: [{ index: 0, delta: { role: 'assistant' }, finish_reason: null }] })}\n\n`,
        `data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: answer }, finish_reason: null }] })}\n\n`,
        `data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`,
        'data: [DONE]\n\n',
      ];
      for (const frame of frames) response.write(frame);
      response.end();
    });
  });
  return new Promise((resolveServer) => {
    server.listen(0, '127.0.0.1', () => resolveServer({ server, port: server.address().port }));
  });
}

function byteScan(haystackText, secrets) {
  const survivors = [];
  for (const [value, label] of secrets) {
    if (haystackText.includes(value)) survivors.push(label);
  }
  return survivors;
}

/** Every fixture text channel INCLUDING decoded frames (base64 hides secrets from a plain scan). */
function allTextChannels(fixture) {
  const decoded = fixture.response.frames.map((f) => Buffer.from(f.data, 'base64').toString('utf8')).join('\n');
  const headers = [...fixture.request.headers, ...fixture.response.headers].map(([, v]) => v).join('\n');
  return [JSON.stringify(fixture), decoded, headers, fixture.request.url, fixture.request.body ?? ''].join('\n');
}

async function selftest() {
  let failures = 0;
  const check = (name, ok, detail = '') => {
    if (ok) console.log(`selftest ok: ${name}`);
    else {
      console.error(`selftest FAIL: ${name}${detail ? ` — ${detail}` : ''}`);
      failures += 1;
    }
  };
  const PLANTED = {
    auth: 'planted-bearer-token-aa11bb22cc33',
    query: 'planted-query-secret-998877',
    cookie: 'planted-cookie-secret-42aabb',
    bodyKey: 'planted-body-key-5566778899',
    env: 'planted-env-value-deadbeef0123',
    frame: 'sk-planted-in-frame-0123456789abcdef',
  };
  const secretsOf = [
    [PLANTED.auth, 'authorization'],
    [PLANTED.query, 'url-param'],
    [PLANTED.cookie, 'cookie'],
    [PLANTED.bodyKey, 'api-key'],
    [PLANTED.env, 'env:FAKTOR_PLANTED_KEY'],
    [PLANTED.frame, 'sk-key'],
  ];

  // ---- Capture A: secrets in headers/url/body only (never in SSE frames).
  {
    const { server, port } = await startSyntheticProvider({});
    try {
      const url = `http://127.0.0.1:${port}/v1/chat/completions?api_key=${PLANTED.query}&stream=true`;
      const raw = await recordExchange({
        url,
        method: 'POST',
        headers: [`Authorization: Bearer ${PLANTED.auth}`, 'content-type: application/json'],
        body: JSON.stringify({ model: 'm', stream: true, api_key: PLANTED.bodyKey, prompt: 'go' }),
        allowHttp: true,
      });
      const originalEvents = parseEvents(
        raw.response.frames.map((f) => Buffer.from(f.data, 'base64').toString('utf8')).join(''),
        'text/event-stream',
      );
      const envValues = [{ key: 'FAKTOR_PLANTED_KEY', value: PLANTED.env }];
      const fixture = scrubCapture(raw, envValues);
      const serialized = JSON.stringify(fixture, null, 2);

      const survivors = byteScan(allTextChannels(fixture), secretsOf.filter(([v]) => v !== PLANTED.frame));
      check('A: zero header/url/body/env secrets survive scrubbing', survivors.length === 0, survivors.join(','));
      check(
        'A: scrub digests recorded without values',
        fixture.scrub.digests.length >= 4 && !fixture.scrub.digests.some((d) => Object.values(d).some((v) => String(v).includes(PLANTED.auth))),
      );

      // Replay byte-exactness + identical parsed-event sequence.
      const replay = await startReplayServer(fixture, { timingScale: 0 });
      try {
        const response = await fetch(`http://127.0.0.1:${replay.port}/anything`);
        const text = await response.text();
        const replayEvents = parseEvents(text, response.headers.get('content-type') ?? '');
        check('A: replay drives the consumer to the identical parsed-event sequence',
          JSON.stringify(replayEvents) === JSON.stringify(originalEvents));
        const originalBytes = raw.response.frames.map((f) => Buffer.from(f.data, 'base64')).reduce((a, b) => Buffer.concat([a, b]), Buffer.alloc(0)).toString('utf8');
        check('A: replay body is byte-identical to the recorded stream (secrets never in frames)', text === originalBytes);
        // tMs is integer ms since the previous frame; the FIRST frame is
        // ms since the request was sent (time-to-first-frame shape).
        check('A: recorded timing shape is normalized integer deltas',
          fixture.response.frames.every((f) => Number.isInteger(f.tMs) && f.tMs >= 0 && f.tMs <= 60_000));
      } finally {
        replay.server.close();
      }
    } finally {
      server.close();
    }
  }

  // ---- Capture B: secrets everywhere, including INSIDE one frame (provider
  // echo), plus a user home path in frame output.
  {
    const { server, port } = await startSyntheticProvider({ echoSecretInFrame: true, homePathInFrame: true });
    try {
      const url = `http://127.0.0.1:${port}/v1/chat/completions`;
      const raw = await recordExchange({
        url,
        method: 'POST',
        headers: [`Authorization: Bearer ${PLANTED.auth}`, 'x-api-key: sk-planted-header-key-00ff112233'],
        body: JSON.stringify({ model: 'm', stream: true }),
        allowHttp: true,
      });
      const envValues = [{ key: 'FAKTOR_PLANTED_KEY', value: PLANTED.env }];
      const fixture = scrubCapture(raw, envValues);
      const channels = allTextChannels(fixture);
      const survivors = byteScan(channels, secretsOf);
      check('B: zero secrets survive even when echoed inside a frame', survivors.length === 0, survivors.join(','));
      const homeSurvivor = channels.match(/\/(?:Users|home)\/[A-Za-z0-9._-]+/);
      const decodedFrames = fixture.response.frames.map((f) => Buffer.from(f.data, 'base64').toString('utf8')).join('\n');
      check('B: user home path is replaced with the placeholder',
        homeSurvivor === null && decodedFrames.includes(HOME_PLACEHOLDER));

      const replay = await startReplayServer(fixture, { timingScale: 0 });
      try {
        const response = await fetch(`http://127.0.0.1:${replay.port}/anything`);
        const text = await response.text();
        const replayEvents = parseEvents(text, 'text/event-stream');
        const scrub = (t) => scrubHomePaths(scrubText(t, secretsOf).text);
        const originalEventsScrubbed = parseEvents(
          scrub(raw.response.frames.map((f) => Buffer.from(f.data, 'base64').toString('utf8')).join('')),
          'text/event-stream',
        );
        // A secret echoed into a frame MUST be rewritten, so equality is
        // modulo the scrub (replay == consumer-of-scrubbed-original).
        check('B: replay equals the scrubbed-original event sequence (mod-scrub equality)',
          JSON.stringify(replayEvents) === JSON.stringify(originalEventsScrubbed));
        check('B: the scrubbed frame still parses as one JSON delta event',
          replayEvents.length >= 2 && replayEvents.some((e) => e.json?.choices?.[0]?.delta?.content?.includes('contract answer')));
        check('B: the secret-echo delta no longer contains the secret',
          !text.includes(PLANTED.frame) && text.includes('«REDACTED:sk-key»'));
      } finally {
        replay.server.close();
      }
    } finally {
      server.close();
    }
  }

  // ---- Adversarial: scrubbing is deterministic (same input -> same output).
  {
    const raw = {
      meta: { recordedAtEpochMs: 1, contentShape: 'sse' },
      request: { method: 'POST', url: 'http://127.0.0.1:1/x?api_key=' + PLANTED.query, headers: [['authorization', `Bearer ${PLANTED.auth}`]], body: '{}' },
      response: {
        status: 200,
        headers: [['set-cookie', `session=${PLANTED.cookie}`]],
        frames: [{ data: Buffer.from(`data: {\"k\":\"${PLANTED.env}\"}\n\n`, 'utf8').toString('base64'), tMs: 0 }],
      },
    };
    const envValues = [{ key: 'FAKTOR_PLANTED_KEY', value: PLANTED.env }];
    const one = JSON.stringify(scrubCapture(raw, envValues));
    const two = JSON.stringify(scrubCapture(raw, envValues));
    check('scrubbing is deterministic', one === two);
    check('deterministic output contains no secret', byteScan(one, secretsOf.filter(([v]) => v !== PLANTED.frame)).length === 0);
  }

  // ---- Adversarial: env-file parser ignores comments/short/invalid lines.
  {
    const parsed = parseEnvFile('# comment\nFAKTOR_X=short\nBADLINE\nFAKTOR_DEEPSEEK_API_KEY=sk-abcdefghijklmnop\n=');
    check('env parser takes only KEY=VALUE with value length >= 8',
      parsed.length === 1 && parsed[0].key === 'FAKTOR_DEEPSEEK_API_KEY');
  }

  // ---- Adversarial: scanner sensitivity — the byte-scan MUST detect every
  // planted secret in the RAW (unscrubbed) capture, or the zero-survivor
  // claims above would be vacuous.
  {
    const { server, port } = await startSyntheticProvider({ echoSecretInFrame: true, homePathInFrame: true });
    try {
      const raw = await recordExchange({
        url: `http://127.0.0.1:${port}/v1/chat/completions?api_key=${PLANTED.query}`,
        method: 'POST',
        headers: [`Authorization: Bearer ${PLANTED.auth}`, 'x-api-key: sk-planted-header-key-00ff112233'],
        body: JSON.stringify({ model: 'm', stream: true, api_key: PLANTED.bodyKey }),
        allowHttp: true,
      });
      const rawChannels = [
        JSON.stringify(raw),
        raw.response.frames.map((f) => Buffer.from(f.data, 'base64').toString('utf8')).join('\n'),
        raw.request.url,
        raw.request.body,
        ...raw.response.headers.map(([, v]) => v),
      ].join('\n');
      const detected = byteScan(rawChannels, secretsOf);
      // PLANTED.env only enters via the env file (deliberately not planted on
      // the wire here); everything else must be detected BEFORE scrubbing.
      const expected = secretsOf.filter(([v]) => v !== PLANTED.env);
      check('scanner detects every wire-planted secret in the raw capture',
        expected.every(([v, label]) => detected.includes(label)) && detected.length >= expected.length - 1,
        `detected=${detected.join(',')}`);
      const scrubbed = scrubCapture(raw, [{ key: 'FAKTOR_PLANTED_KEY', value: PLANTED.env }]);
      check('the same scan finds zero survivors after scrubbing',
        byteScan(allTextChannels(scrubbed), secretsOf).length === 0);
    } finally {
      server.close();
    }
  }

  if (failures > 0) {
    console.error(`record-replay selftest: FAIL (${failures})`);
    return 1;
  }
  console.log('record-replay selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1]);
if (isMain) {
  main().then((code) => {
    if (typeof code === 'number') process.exit(code);
  }).catch((error) => {
    console.error(`record-replay: ${error?.message ?? error}`);
    if (process.env.HARNESS_DEBUG) console.error(error.stack);
    process.exit(1);
  });
}
