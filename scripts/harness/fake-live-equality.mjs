#!/usr/bin/env node
// TEST-SEPARATION: oracle=differential isolation=separate-process
//
// HAR-15 fake-vs-live equality lane.
//
// ONE contract test definition (the turn shapes below) is run twice:
//   * fake leg  — in-process loopback server, deterministic, OFFLINE, always
//                 runs;
//   * live leg  — real provider, env-gated (`--live` + key file), cost-
//                 bounded (request cap + small max_tokens), skipped with a
//                 note when the env file or flag is absent.
//
// The two legs are compared on the PARSED event stream SEMANTICS, not bytes.
// Normalization rules (also emitted into the JSON report):
//   N1 chunking ignored   — response bytes are concatenated before UTF-8
//                           decode and SSE parse; chunk/frame boundaries and
//                           timings never participate in equality. This is
//                           what makes a multi-byte character split across
//                           chunks reassemble identically on both legs.
//   N2 delta collapse     — role/content deltas collapse to one accumulated
//                           `message` event (or `tool_call` per call index);
//                           providers differ in how many chunks they emit.
//   N3 ids/usage ignored  — ids, model names, created timestamps, token
//                           usage are never compared.
//   N4 tool payloads      — compared as name + args that SATISFY THE DECLARED
//                           JSON SCHEMA (required keys, value types), not as
//                           byte-equal argument strings (models phrase
//                           arguments differently).
//   N5 content classes    — compared as contentClass ('nonempty-text'),
//                           hasMultibyte, hasReplacementChar; free text is
//                           compared only where the contract pins it (fake).
//   N6 error contract     — both legs must yield exactly one `error` event
//                           with integer status >= 400, messageClass 'string'
//                           and retryAfterS (number|null). The 429+
//                           Retry-After KIND is deterministic only on the
//                           fake (rate_limited, retryAfterS 1); the live leg
//                           elicits a genuine 4xx and is asserted to the
//                           same error-contract shape, not to the same kind.
//   N7 finish reasons     — provider finish_reason strings normalized to
//                           stop|length|tool_calls|content_filter|unknown.
//
// Usage:
//   node scripts/harness/fake-live-equality.mjs                  # fake leg only
//   node scripts/harness/fake-live-equality.mjs --live           # + live leg
//   node scripts/harness/fake-live-equality.mjs --live --report target/harness/equality.json
//   node scripts/harness/fake-live-equality.mjs --live --live-env <path>  # env file override
//   node scripts/harness/fake-live-equality.mjs selftest         # adversarial parser/normalizer checks
//
// Exit codes: 0 all executed legs passed (skipped live is not a failure);
//             1 any executed leg failed; 2 usage error.

import { createHash } from 'node:crypto';
import { createServer } from 'node:http';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const SCHEMA = 'faktor.harness.fake-live-equality/v1';
const DEFAULT_ENV_FILE = resolve(homedir(), '.faktor-live/deepseek.env');
const DEFAULT_LIVE_CAP = 6; // hard request cap for the whole live leg
const REQUEST_TIMEOUT_MS = 30_000;

export const NORMALIZATION = Object.freeze([
  'N1 chunking ignored: bytes concatenated before decode/parse; boundaries and timing never compared',
  'N2 delta collapse: role/content deltas collapse to one accumulated message event; tool-call chunks accumulate per index',
  'N3 ids/usage ignored: ids, model names, timestamps, token usage never compared',
  'N4 tool payload semantics: name + args satisfying the declared JSON schema (required keys, value types), not byte-equal args',
  'N5 content classes: contentClass/hasReplacementChar always compared; hasMultibyte compared ONLY for utf8-split-stream (the case whose contract pins multibyte on both legs — free text may or may not contain multibyte codepoints, e.g. an em-dash, so it cannot be a cross-leg field for plain replies)',
  'N6 error contract: exactly one error event, integer status>=400, messageClass=string, retryAfterS number|null; cross-leg equality compares statusClass+messageClass only — the 429+Retry-After presence is fake-deterministic (contract-asserted per leg: fake pins rate_limited/retryAfterS 1, live elicits a genuine 4xx), so retryAfter presence and kind are per-leg assertions, not cross-leg equality fields',
  'N7 finish reasons normalized to stop|length|tool_calls|content_filter|unknown',
]);

/** The contract: turn shapes every leg must execute. */
export const CASES = Object.freeze(['plain-chat', 'tool-call-turn', 'utf8-split-stream', 'error-shape']);

const TOOL_DEF = {
  type: 'function',
  function: {
    name: 'get_weather',
    description: 'Get the current weather for a city.',
    parameters: {
      type: 'object',
      properties: {
        city: { type: 'string' },
        unit: { type: 'string', enum: ['celsius', 'fahrenheit'] },
      },
      required: ['city'],
    },
  },
};
const UTF8_MARKER = 'HAR15-世界Ω-marker';
const NON_ASCII_RE = /[^\x00-\x7F]/;

// ---------------------------------------------------------------- parsing

function normalizeFinish(reason) {
  if (reason === 'stop' || reason === 'length' || reason === 'tool_calls' || reason === 'content_filter') return reason;
  return 'unknown';
}

/** Does an argument object satisfy the declared tool schema (required + types)? */
export function argsSatisfySchema(args, schema) {
  if (typeof args !== 'object' || args === null || Array.isArray(args)) return false;
  for (const key of schema.required ?? []) {
    if (!Object.prototype.hasOwnProperty.call(args, key)) return false;
  }
  for (const [key, prop] of Object.entries(schema.properties ?? {})) {
    if (!Object.prototype.hasOwnProperty.call(args, key)) continue;
    const value = args[key];
    if (prop.type === 'string' && typeof value !== 'string') return false;
    if (prop.type === 'number' && typeof value !== 'number') return false;
    if (prop.enum && !prop.enum.includes(value)) return false;
  }
  return true;
}

/**
 * Parse one HTTP exchange into the NORMALIZED semantic event sequence.
 * `rawBytes` is the full response body (concatenated — rule N1).
 */
export function normalizeExchange({ status, headers, rawBytes }) {
  const text = Buffer.from(rawBytes).toString('utf8');
  if (status >= 400) {
    const headerGet = typeof headers?.get === 'function' ? headers.get.bind(headers) : (name) => headers?.[name] ?? null;
    const retryAfterRaw = headerGet('retry-after');
    const retryAfterS = retryAfterRaw === null || retryAfterRaw === undefined || retryAfterRaw === ''
      ? null
      : Number(retryAfterRaw);
    let messageClass = 'non-json';
    try {
      const parsed = JSON.parse(text);
      const message = parsed?.error?.message ?? parsed?.message;
      messageClass = typeof message === 'string' && message.length > 0 ? 'string' : 'non-string';
    } catch {
      messageClass = 'non-json';
    }
    return {
      events: [
        {
          type: 'error',
          kind: status === 429 ? 'rate_limited' : 'http_error',
          status: Number(status),
          retryAfterS: Number.isFinite(retryAfterS) ? retryAfterS : null,
          messageClass,
        },
      ],
      raw: text,
    };
  }
  const headerGet = typeof headers?.get === 'function' ? headers.get.bind(headers) : (name) => headers?.[name] ?? '';
  const contentType = headerGet('content-type') ?? '';
  let body;
  if (contentType.includes('event-stream')) {
    body = parseSseBody(text);
  } else {
    try {
      body = JSON.parse(text);
    } catch {
      return {
        events: [
          {
            type: 'error',
            kind: 'parse_error',
            status: 200,
            retryAfterS: null,
            messageClass: 'non-json',
          },
        ],
        raw: text,
      };
    }
  }
  const choice = body?.choices?.[0] ?? {};
  const message = choice.message ?? {};
  const finish = normalizeFinish(choice.finish_reason ?? null);
  const events = [];
  const toolCalls = message.tool_calls ?? [];
  if (toolCalls.length > 0) {
    for (const call of toolCalls) {
      let args;
      try {
        args = JSON.parse(call.function?.arguments ?? '{}');
      } catch {
        args = { __unparsed: true };
      }
      events.push({
        type: 'tool_call',
        name: call.function?.name ?? '',
        args,
        argsSatisfySchema: argsSatisfySchema(args, TOOL_DEF.function.parameters),
      });
    }
  } else {
    const content = typeof message.content === 'string' ? message.content : '';
    events.push({
      type: 'message',
      role: message.role ?? 'assistant',
      text: content,
      contentClass: content.length > 0 ? 'nonempty-text' : 'empty-text',
      hasMultibyte: NON_ASCII_RE.test(content),
      hasReplacementChar: content.includes('�'),
    });
  }
  events.push({ type: 'final', finishReason: finish });
  return { events, raw: text };
}

/** SSE body -> accumulated JSON object (role/content/tool_calls/finish_reason). */
function parseSseBody(text) {
  const accumulated = { choices: [{ message: { role: 'assistant', content: '', tool_calls: [] }, finish_reason: null }] };
  const toolMap = new Map();
  for (const line of text.split('\n')) {
    if (!line.startsWith('data:')) continue;
    const payload = line.slice(5).trim();
    if (payload === '' || payload === '[DONE]') continue;
    let frame;
    try {
      frame = JSON.parse(payload);
    } catch {
      continue; // torn/unknown frame after N1 concatenation: skip loudly, keep parsing
    }
    const choice = frame?.choices?.[0];
    if (!choice) continue;
    const delta = choice.delta ?? {};
    if (delta.role) accumulated.choices[0].message.role = delta.role;
    if (typeof delta.content === 'string') accumulated.choices[0].message.content += delta.content;
    for (const call of delta.tool_calls ?? []) {
      const index = call.index ?? 0;
      if (!toolMap.has(index)) {
        toolMap.set(index, { id: '', function: { name: '', arguments: '' } });
      }
      const acc = toolMap.get(index);
      if (call.id) acc.id = call.id;
      if (call.function?.name) acc.function.name += call.function.name;
      if (call.function?.arguments) acc.function.arguments += call.function.arguments;
    }
    if (choice.finish_reason) accumulated.choices[0].finish_reason = choice.finish_reason;
  }
  if (toolMap.size > 0) {
    accumulated.choices[0].message.tool_calls = [...toolMap.entries()]
      .sort((a, b) => a[0] - b[0])
      .map(([, v]) => v);
  }
  return accumulated;
}

// ---------------------------------------------------------------- contract

function expectSequence(events, leg, spec) {
  const failures = [];
  const types = events.map((e) => e.type);
  if (JSON.stringify(types) !== JSON.stringify(spec.types)) {
    failures.push(`event types ${JSON.stringify(types)} != ${JSON.stringify(spec.types)}`);
    return failures;
  }
  if (spec.message) {
    const message = events[0];
    for (const [key, expected] of Object.entries(spec.message)) {
      if (message[key] !== expected) {
        failures.push(`message.${key}=${JSON.stringify(message[key])} != ${JSON.stringify(expected)}`);
      }
    }
  }
  if (spec.pinnedText !== null && spec.pinnedText !== undefined) {
    if (events[0].text !== spec.pinnedText) {
      failures.push(`pinned text ${JSON.stringify(events[0].text)} != ${JSON.stringify(spec.pinnedText)}`);
    }
  }
  if (spec.toolCall) {
    const call = events[0];
    if (call.name !== spec.toolCall.name) failures.push(`tool name ${call.name} != ${spec.toolCall.name}`);
    if (call.argsSatisfySchema !== spec.toolCall.argsSatisfySchema) {
      failures.push('tool args do not satisfy the declared schema');
    }
  }
  if (spec.final) {
    const fin = events[events.length - 1];
    if (fin.finishReason !== spec.final.finishReason) {
      failures.push(`finish ${fin.finishReason} != ${spec.final.finishReason}`);
    }
  }
  return failures;
}

export function buildRequests() {
  return {
    'plain-chat': {
      body: {
        messages: [{ role: 'user', content: 'Reply with a one-line greeting for the HAR-15 contract run.' }],
        stream: false,
        max_tokens: 64,
      },
      expect: (events, leg) =>
        expectSequence(events, leg, {
          types: ['message', 'final'],
          message: { contentClass: 'nonempty-text', hasReplacementChar: false },
          final: { finishReason: 'stop' },
          pinnedText: leg === 'fake' ? 'HAR-15 contract reply' : null,
        }),
    },
    'tool-call-turn': {
      body: {
        messages: [{ role: 'user', content: 'What is the current weather in Paris? Use the get_weather tool.' }],
        tools: [TOOL_DEF],
        tool_choice: { type: 'function', function: { name: 'get_weather' } },
        stream: false,
        max_tokens: 128,
      },
      expect: (events) =>
        expectSequence(events, 'any', {
          types: ['tool_call', 'final'],
          toolCall: { name: 'get_weather', argsSatisfySchema: true },
          final: { finishReason: 'tool_calls' },
        }),
    },
    'utf8-split-stream': {
      body: {
        messages: [{ role: 'user', content: `Reply with exactly this and nothing else: ${UTF8_MARKER}` }],
        stream: true,
        max_tokens: 64,
      },
      expect: (events, leg) =>
        expectSequence(events, leg, {
          types: ['message', 'final'],
          message: { contentClass: 'nonempty-text', hasMultibyte: true, hasReplacementChar: false },
          final: { finishReason: 'stop' },
          pinnedText: leg === 'fake' ? UTF8_MARKER : null,
        }),
    },
    'error-shape': {
      body: { messages: [{ role: 'user', content: 'hi' }], stream: false, max_tokens: 8 },
      expect: (events, leg) => {
        const failures = [];
        if (events.length !== 1 || events[0].type !== 'error') {
          failures.push(`expected exactly one error event, got ${JSON.stringify(events.map((e) => e.type))}`);
          return failures;
        }
        const error = events[0];
        if (!Number.isInteger(error.status) || error.status < 400) failures.push(`bad error status ${error.status}`);
        if (error.messageClass !== 'string') failures.push(`error messageClass ${error.messageClass} != string`);
        if (!(error.retryAfterS === null || typeof error.retryAfterS === 'number')) {
          failures.push('retryAfterS not number|null');
        }
        if (leg === 'fake' && (error.kind !== 'rate_limited' || error.retryAfterS !== 1)) {
          failures.push(`fake error must be rate_limited with retryAfterS 1, got ${error.kind}/${error.retryAfterS}`);
        }
        if (leg === 'live' && error.kind !== 'http_error' && error.kind !== 'rate_limited') {
          failures.push(`live error kind ${error.kind} outside the contract`);
        }
        return failures;
      },
    },
  };
}

/** Cross-leg equality on NORMALIZED semantics (rules N1–N7). */
export function compareNormalized(fakeEvents, liveEvents, { compareMultibyte = false } = {}) {
  const shape = (events) =>
    events.map((event) => {
      const copy = { type: event.type };
      if (event.type === 'message') {
        copy.contentClass = event.contentClass;
        copy.hasReplacementChar = event.hasReplacementChar;
        copy.role = event.role;
        if (compareMultibyte) copy.hasMultibyte = event.hasMultibyte;
      } else if (event.type === 'tool_call') {
        copy.name = event.name;
        copy.argsSchemaOk = event.argsSatisfySchema;
        copy.argKeysTyped = Object.entries(event.args ?? {})
          .map(([k, v]) => `${k}:${typeof v}`)
          .sort();
      } else if (event.type === 'final') {
        copy.finishReason = event.finishReason;
      } else if (event.type === 'error') {
        copy.statusClass = event.status >= 400 ? '4xx-or-5xx' : 'unexpected';
        copy.messageClass = event.messageClass;
      }
      return copy;
    });
  const a = JSON.stringify(shape(fakeEvents));
  const b = JSON.stringify(shape(liveEvents));
  if (a !== b) return [`normalized shapes differ:\n  fake: ${a}\n  live: ${b}`];
  return [];
}

// ---------------------------------------------------------------- legs

/** Deterministic in-process loopback fake implementing all four turn shapes. */
export function startContractFake() {
  const server = createServer(async (request, response) => {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    let body = {};
    try {
      body = JSON.parse(Buffer.concat(chunks).toString('utf8') || '{}');
    } catch {
      response.writeHead(400, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ error: { message: 'invalid json' } }));
      return;
    }
    const model = String(body.model ?? '');
    if (model.includes('does-not-exist')) {
      // Deterministic error shape: 429 + Retry-After on the fake leg.
      response.writeHead(429, { 'content-type': 'application/json', 'retry-after': '1' });
      response.end(
        JSON.stringify({ error: { message: 'Rate limit reached for contract fake', type: 'rate_limit_error' } }),
      );
      return;
    }
    if (Array.isArray(body.tools) && body.tools.length > 0) {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(
        JSON.stringify({
          id: 'chatcmpl-har15-tool',
          object: 'chat.completion',
          model,
          choices: [
            {
              index: 0,
              message: {
                role: 'assistant',
                content: null,
                tool_calls: [
                  {
                    id: 'call_har15',
                    type: 'function',
                    function: { name: 'get_weather', arguments: JSON.stringify({ city: 'Paris', unit: 'celsius' }) },
                  },
                ],
              },
              finish_reason: 'tool_calls',
            },
          ],
          usage: { prompt_tokens: 10, completion_tokens: 8, total_tokens: 18 },
        }),
      );
      return;
    }
    if (body.stream === true) {
      response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
      const markerFrame = `data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: UTF8_MARKER }, finish_reason: null }] })}\n\n`;
      const finishFrame = `data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`;
      const bytes = Buffer.from(`${markerFrame}${finishFrame}data: [DONE]\n\n`, 'utf8');
      // Split INSIDE the 3-byte char 世 so a frame boundary lands mid-codepoint.
      const charAt = bytes.indexOf(Buffer.from('世', 'utf8'));
      const at = charAt + 1;
      response.write(bytes.subarray(0, at));
      await new Promise((r) => setTimeout(r, 5));
      response.write(bytes.subarray(at));
      response.end();
      return;
    }
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(
      JSON.stringify({
        id: 'chatcmpl-har15-plain',
        object: 'chat.completion',
        model,
        choices: [
          { index: 0, message: { role: 'assistant', content: 'HAR-15 contract reply' }, finish_reason: 'stop' },
        ],
        usage: { prompt_tokens: 9, completion_tokens: 5, total_tokens: 14 },
      }),
    );
  });
  return new Promise((resolveServer) => {
    server.listen(0, '127.0.0.1', () => resolveServer({ server, port: server.address().port }));
  });
}

async function postChat({ baseUrl, apiKey, path, body, model }) {
  const payload = { ...body, model: model ?? body.model ?? 'm' };
  const headers = { 'content-type': 'application/json' };
  if (apiKey) headers.authorization = `Bearer ${apiKey}`;
  const response = await fetch(`${baseUrl}${path}`, {
    method: 'POST',
    headers,
    body: JSON.stringify(payload),
    signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    redirect: 'error',
  });
  const rawBytes = Buffer.from(await response.arrayBuffer());
  return { status: response.status, headers: response.headers, rawBytes };
}

function makeRunner({ baseUrl, apiKey, model, path }) {
  return async (caseName, budget) => {
    const spec = buildRequests()[caseName];
    if (!spec) throw new Error(`unknown case ${caseName}`);
    if (budget) {
      if (budget.used >= budget.cap) return { status: 'skipped', note: `live request cap ${budget.cap} reached` };
      budget.used += 1;
    }
    const exchange = await postChat({ baseUrl, apiKey, path, body: { ...spec.body, model } });
    const { events, raw } = normalizeExchange(exchange);
    const failures = spec.expect(events, budget ? 'live' : 'fake');
    return {
      status: failures.length === 0 ? 'passed' : 'failed',
      failures,
      events,
      responseBytes: exchange.rawBytes.length,
      note: undefined,
    };
  };
}

// ---------------------------------------------------------------- main

async function main() {
  const args = process.argv.slice(2);
  if (args[0] === 'selftest') return selftest();
  const value = (name, fallback) => {
    const at = args.indexOf(`--${name}`);
    return at >= 0 && at + 1 < args.length ? args[at + 1] : fallback;
  };
  const known = ['--live', '--report', '--live-env', '--base-url', '--max-live-requests'];
  const unknown = args.filter((a) => a.startsWith('--') && !known.includes(a));
  if (unknown.length > 0) {
    console.error(`unknown flags: ${unknown.join(' ')}`);
    return 2;
  }
  const wantLive = args.includes('--live');

  const report = {
    schema: SCHEMA,
    generatedAtEpochMs: Date.now(),
    normalization: [...NORMALIZATION],
    budget: { liveRequestCap: Number(value('max-live-requests', String(DEFAULT_LIVE_CAP))), liveRequestsUsed: 0 },
    legs: {},
    equality: { result: 'not-run' },
  };

  // ---- fake leg (offline, always)
  {
    const { server, port } = await startContractFake();
    try {
      const base = { baseUrl: `http://127.0.0.1:${port}`, apiKey: null, path: '/v1/chat/completions' };
      const runner = makeRunner({ ...base, model: 'contract-fake' });
      const errorRunner = makeRunner({ ...base, model: 'faktor-har15-model-does-not-exist' });
      const cases = [];
      for (const caseName of CASES) {
        try {
          // Same turn-shape dispatch as the live leg: error-shape targets the
          // model name the fake answers with 429 + Retry-After.
          const result = await (caseName === 'error-shape' ? errorRunner : runner)(caseName, null);
          cases.push({ name: caseName, ...result });
        } catch (error) {
          cases.push({ name: caseName, status: 'failed', failures: [String(error?.message ?? error)] });
        }
      }
      report.legs.fake = { status: cases.every((c) => c.status === 'passed') ? 'passed' : 'failed', cases };
    } finally {
      server.close();
    }
  }

  // ---- live leg (env-gated)
  if (wantLive) {
    // NOTE: named --live-env, not --env-file — Node itself intercepts
    // `--env-file` anywhere in argv and would try to load it as dotenv.
    const envFile = value('live-env', DEFAULT_ENV_FILE);
    if (!existsSync(envFile)) {
      report.legs.live = {
        status: 'skipped',
        note: `env file absent: ${envFile} (read-only expected location); live leg SKIP`,
      };
    } else {
      const env = {};
      for (const line of readFileSync(envFile, 'utf8').split('\n')) {
        const trimmed = line.trim();
        if (!trimmed || trimmed.startsWith('#')) continue;
        const at = trimmed.indexOf('=');
        if (at > 0) env[trimmed.slice(0, at).trim()] = trimmed.slice(at + 1).trim();
      }
      const apiKey = env.FAKTOR_DEEPSEEK_API_KEY ?? null;
      if (!apiKey) {
        report.legs.live = {
          status: 'skipped',
          note: `FAKTOR_DEEPSEEK_API_KEY not present in ${envFile}; live leg SKIP`,
        };
      } else {
        const baseUrl = value('base-url', process.env.FAKTOR_LIVE_BASE_URL ?? 'https://api.deepseek.com');
        const budget = { used: 0, cap: report.budget.liveRequestCap };
        const runner = makeRunner({ baseUrl, apiKey, model: 'deepseek-chat', path: '/chat/completions' });
        const errorRunner = makeRunner({
          baseUrl,
          apiKey,
          model: 'faktor-har15-model-does-not-exist',
          path: '/chat/completions',
        });
        const cases = [];
        for (const caseName of CASES) {
          try {
            // error-shape elicits a genuine provider 4xx (nonexistent model)
            // so no completion tokens are burned on it.
            const result = await (caseName === 'error-shape' ? errorRunner : runner)(caseName, budget);
            cases.push({ name: caseName, ...result });
          } catch (error) {
            cases.push({ name: caseName, status: 'failed', failures: [String(error?.message ?? error)] });
          }
        }
        report.budget.liveRequestsUsed = budget.used;
        report.legs.live = {
          status: cases.every((c) => c.status === 'passed') ? 'passed' : 'failed',
          baseUrl,
          keyDigest: `sha256:${createHash('sha256').update(apiKey).digest('hex').slice(0, 12)}`,
          requestsUsed: budget.used,
          cases,
        };
      }
    }
  }

  // ---- cross-leg equality on normalized semantics
  if (report.legs.fake && report.legs.live && report.legs.live.status !== 'skipped') {
    const failures = [];
    for (const caseName of CASES) {
      const fakeCase = report.legs.fake.cases.find((c) => c.name === caseName);
      const liveCase = report.legs.live.cases.find((c) => c.name === caseName);
      if (!fakeCase || !liveCase || fakeCase.status !== 'passed' || liveCase.status !== 'passed') continue;
      failures.push(
        ...compareNormalized(fakeCase.events ?? [], liveCase.events ?? [], {
          compareMultibyte: caseName === 'utf8-split-stream',
        }).map((f) => `${caseName}: ${f}`),
      );
    }
    report.equality = failures.length === 0 ? { result: 'equal' } : { result: 'not-equal', failures };
  } else if (report.legs.live?.status === 'skipped') {
    report.equality = { result: 'not-run', note: report.legs.live.note };
  }

  const reportPath = value('report', null);
  if (reportPath) {
    const path = resolve(reportPath);
    mkdirSync(dirname(path), { recursive: true });
    writeFileSync(path, `${JSON.stringify(report, null, 2)}\n`);
    console.log(`fake-live-equality: report -> ${reportPath}`);
  } else {
    console.log(JSON.stringify(report, null, 2));
  }

  const fakeFailed = report.legs.fake.status !== 'passed';
  const liveFailed = report.legs.live && report.legs.live.status === 'failed';
  const equalityFailed = report.equality.result === 'not-equal';
  if (fakeFailed || liveFailed || equalityFailed) {
    console.error('fake-live-equality: FAIL');
    return 1;
  }
  const liveNote = report.legs.live?.status === 'skipped' ? ` (live: SKIPPED — ${report.legs.live.note})` : '';
  console.log(
    `fake-live-equality: PASS — fake ${report.legs.fake.status}, live ${report.legs.live?.status ?? 'not-run'}${liveNote}`,
  );
  return 0;
}

// ---------------------------------------------------------------- selftest

function selftest() {
  let failures = 0;
  const check = (name, ok, detail = '') => {
    if (ok) console.log(`selftest ok: ${name}`);
    else {
      console.error(`selftest FAIL: ${name}${detail ? ` — ${detail}` : ''}`);
      failures += 1;
    }
  };

  // Adversarial: UTF-8 char split across chunks must reassemble (N1).
  {
    const marker = `x${'世'}y`;
    const fullBytes = Buffer.from(
      `data: ${JSON.stringify({ choices: [{ index: 0, delta: { content: marker }, finish_reason: null }] })}\n\n` +
        `data: ${JSON.stringify({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }] })}\n\n`,
      'utf8',
    );
    const charAt = fullBytes.indexOf(Buffer.from('世', 'utf8'));
    const at = charAt + 1;
    const joined = Buffer.concat([fullBytes.subarray(0, at), fullBytes.subarray(at)]);
    const { events } = normalizeExchange({ status: 200, headers: { get: () => 'text/event-stream' }, rawBytes: joined });
    check('split multi-byte char reassembles without replacement',
      events[0].text === marker && !events[0].hasReplacementChar, JSON.stringify(events[0]));
    check('the split really was mid-codepoint', at > charAt);
    const partial = fullBytes.subarray(0, at).toString('utf8');
    check('control: decoding the torn half alone corrupts (probe is sensitive)', partial.includes('�') || !partial.includes('世'));
  }

  // Adversarial: torn/unknown SSE frames are skipped, valid ones parse.
  {
    const body =
      'data: not-json-at-all\n\n' +
      'data: {"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}\n\n' +
      'data: {"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":null}]}\n\n' +
      'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n' +
      'data: [DONE]\n\n';
    const { events } = normalizeExchange({ status: 200, headers: { get: () => 'text/event-stream' }, rawBytes: Buffer.from(body) });
    check('torn frame skipped, valid stream still normalized',
      JSON.stringify(events.map((e) => e.type)) === JSON.stringify(['message', 'final']) && events[0].text === 'ok');
  }

  // Adversarial: illegal finish reason normalizes to unknown; empty content class.
  {
    const body = JSON.stringify({
      choices: [{ index: 0, message: { role: 'assistant', content: '' }, finish_reason: 'weird-provider-reason' }],
    });
    const { events } = normalizeExchange({ status: 200, headers: { get: () => 'application/json' }, rawBytes: Buffer.from(body) });
    check('empty content classified and unknown finish normalized',
      events[0].contentClass === 'empty-text' && events[1].finishReason === 'unknown');
  }

  // Adversarial: error contract — malformed body, 429 with Retry-After, missing header.
  {
    const e1 = normalizeExchange({ status: 429, headers: { get: (h) => (h === 'retry-after' ? '1' : null) }, rawBytes: Buffer.from('not json') });
    check('429 with Retry-After is rate_limited with messageClass non-json',
      e1.events[0].kind === 'rate_limited' && e1.events[0].retryAfterS === 1 && e1.events[0].messageClass === 'non-json');
    const e2 = normalizeExchange({ status: 500, headers: { get: () => null }, rawBytes: Buffer.from(JSON.stringify({ error: { message: 'boom' } })) });
    check('5xx without Retry-After is http_error with retryAfterS null',
      e2.events[0].kind === 'http_error' && e2.events[0].retryAfterS === null && e2.events[0].messageClass === 'string');
    const e3 = normalizeExchange({ status: 400, headers: { get: () => null }, rawBytes: Buffer.from('') });
    check('empty error body still yields a contract error event',
      e3.events[0].status === 400 && e3.events[0].messageClass === 'non-json');
  }

  // Adversarial: tool args violating the schema must fail argsSatisfySchema.
  {
    check('args missing required key rejected', !argsSatisfySchema({ unit: 'celsius' }, TOOL_DEF.function.parameters));
    check('args wrong type rejected', !argsSatisfySchema({ city: 42 }, TOOL_DEF.function.parameters));
    check('args bad enum rejected', !argsSatisfySchema({ city: 'Paris', unit: 'kelvin' }, TOOL_DEF.function.parameters));
    check('valid args accepted', argsSatisfySchema({ city: 'Paris' }, TOOL_DEF.function.parameters));
    check('non-object args rejected', !argsSatisfySchema(null, TOOL_DEF.function.parameters) && !argsSatisfySchema([], TOOL_DEF.function.parameters));
  }

  // Adversarial: comparison ignores free text but not structure (N2/N5/N7).
  {
    const a = [
      { type: 'message', role: 'assistant', text: 'hello wörld', contentClass: 'nonempty-text', hasMultibyte: true, hasReplacementChar: false },
      { type: 'final', finishReason: 'stop' },
    ];
    const b = [
      { type: 'message', role: 'assistant', text: 'HELLO WÖRLD DIFFERENT', contentClass: 'nonempty-text', hasMultibyte: true, hasReplacementChar: false },
      { type: 'final', finishReason: 'stop' },
    ];
    check('normalized equality ignores free text (N5)', compareNormalized(a, b).length === 0);
    const c = [
      { type: 'message', role: 'assistant', text: 'x', contentClass: 'nonempty-text', hasMultibyte: false, hasReplacementChar: false },
      { type: 'final', finishReason: 'length' },
    ];
    check('normalized equality catches finish-reason drift (N7)', compareNormalized(a, c).length === 1);
    const d = [a[0], { type: 'tool_call', name: 'x', args: {}, argsSatisfySchema: false }, a[1]];
    check('normalized equality catches event-sequence drift', compareNormalized(a, d).length === 1);
    const e = [a[0], { type: 'final', finishReason: 'stop', provider: 'leaky-field' }];
    check('normalized equality ignores extra provider fields (N3)', compareNormalized(a, e).length === 0);
    const f = [{ type: 'tool_call', name: 'x', args: { city: 1 }, argsSatisfySchema: false }, a[1]];
    check('normalized equality catches tool-args class drift (N4)',
      compareNormalized(
        [{ type: 'tool_call', name: 'x', args: { city: 'x' }, argsSatisfySchema: true }, a[1]],
        f,
      ).length === 1);
    // N5: multibyte participates ONLY for the utf8-split-stream shape.
    const mbLive = [{ ...a[0], hasMultibyte: false }, a[1]];
    check('multibyte drift ignored for plain replies (N5 default)',
      compareNormalized(a, mbLive).length === 0);
    check('multibyte drift compared for the utf8-split-stream shape (N5 utf8)',
      compareNormalized(a, mbLive, { compareMultibyte: true }).length === 1);
    // N6: 429 Retry-After presence is a per-leg contract assertion, not a
    // cross-leg equality field (live elicits a genuine 4xx without it).
    const errFake = [{ type: 'error', kind: 'rate_limited', status: 429, retryAfterS: 1, messageClass: 'string' }];
    const errLive = [{ type: 'error', kind: 'http_error', status: 400, retryAfterS: null, messageClass: 'string' }];
    check('429-vs-4xx error shapes are equal cross-leg (N6)',
      compareNormalized(errFake, errLive).length === 0);
    check('error statusClass drift still caught',
      compareNormalized(errFake, [{ type: 'error', kind: 'http_error', status: 503, retryAfterS: null, messageClass: 'non-json' }]).length === 1);
  }

  // Adversarial: contract expectations reject wrong sequences.
  {
    const spec = buildRequests()['plain-chat'];
    check('plain-chat contract rejects a message-less sequence',
      spec.expect([{ type: 'final', finishReason: 'stop' }], 'fake').length > 0);
    const toolSpec = buildRequests()['tool-call-turn'];
    const wrongTool = [
      { type: 'tool_call', name: 'get_weather', args: { unit: 'celsius' }, argsSatisfySchema: false },
      { type: 'final', finishReason: 'tool_calls' },
    ];
    check('tool contract rejects schema-violating args', toolSpec.expect(wrongTool, 'fake').some((f) => f.includes('schema')));
    const errSpec = buildRequests()['error-shape'];
    check('error contract accepts the fake 429 shape',
      errSpec.expect([{ type: 'error', kind: 'rate_limited', status: 429, retryAfterS: 1, messageClass: 'string' }], 'fake').length === 0);
    check('error contract rejects a non-error sequence',
      errSpec.expect([{ type: 'message', role: 'assistant', text: 'x', contentClass: 'nonempty-text', hasMultibyte: false, hasReplacementChar: false }], 'fake').length > 0);
  }

  if (failures > 0) {
    console.error(`fake-live-equality selftest: FAIL (${failures})`);
    return 1;
  }
  console.log('fake-live-equality selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && fileURLToPath(import.meta.url) === resolve(process.argv[1]);
if (isMain) {
  main()
    .then((code) => {
      if (typeof code === 'number') process.exit(code);
    })
    .catch((error) => {
      console.error(`fake-live-equality: ${error?.message ?? error}`);
      if (process.env.HARNESS_DEBUG) console.error(error.stack);
      process.exit(1);
    });
}
