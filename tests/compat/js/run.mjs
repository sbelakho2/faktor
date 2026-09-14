// Unmodified-client replay driver.
//
// Runs the VERBATIM vendored upstream client (`@kilocode/sdk@7.5.6`,
// compat/kilo-v756/upstream-sdk) against a real Faktor daemon over the wire.
// Every request is recorded from inside the client's own transport hook
// (method, URL+query, headers, body bytes); every response is recorded as it
// crossed the wire (status, content-type, body). The driver edits nothing in
// the upstream tree: the `.js`->`.ts` resolution lives in resolve-ts.mjs and
// the recording transport is injected through the SDK's public `fetch`
// config.
//
//   KILO_BASE       daemon origin (required)
//   KILO_PASSWORD   server password (required)
//   KILO_TRACES     directory of trace-group JSON files (required)
//   KILO_OUT        output JSON path (required)
//
// Exit code 0: traces written (per-step failures are recorded, not fatal).

import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { createKiloClient } from "../../../compat/kilo-v756/upstream-sdk/src/v2/client.ts";

const BASE = process.env.KILO_BASE;
const PASSWORD = process.env.KILO_PASSWORD;
const TRACES = process.env.KILO_TRACES;
const OUT = process.env.KILO_OUT;
if (!BASE || !PASSWORD || !TRACES || !OUT) {
  console.error("KILO_BASE, KILO_PASSWORD, KILO_TRACES and KILO_OUT are required");
  process.exit(2);
}

const STEP_TIMEOUT_MS = 15_000;
const SSE_WINDOW_MS = 600;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

if (typeof AbortSignal.timeout !== "function") {
  console.error("node >= 17 required (AbortSignal.timeout)");
  process.exit(2);
}

/** Steps currently being recorded (the recording transport appends here). */
let current = null;

function recordingFetch(input, init) {
  const request = input instanceof Request ? input : new Request(input, init);
  const url = new URL(request.url);
  const record = {
    method: request.method,
    url: request.url,
    path: url.pathname,
    query: url.search.startsWith("?") ? url.search.slice(1) : url.search,
    headers: Object.fromEntries(
      [...request.headers.entries()].map(([name, value]) => [name.toLowerCase(), value]),
    ),
    body: null,
  };
  current.requests.push(record);
  return (async () => {
    if (request.body) {
      record.body = await request.clone().text();
    }
    const response = await fetch(request, { duplex: "half" });
    const contentType = response.headers.get("content-type") ?? "";
    const eventStream = contentType.includes("text/event-stream");
    const responseRecord = {
      status: response.status,
      content_type: contentType,
      headers: Object.fromEntries(
        [...response.headers.entries()].map(([name, value]) => [name.toLowerCase(), value]),
      ),
      body: null,
      event_stream: eventStream,
    };
    current.responses.push(responseRecord);
    if (!eventStream) {
      responseRecord.body = await response.clone().text();
    }
    return response;
  })();
}

const authHeaders = {
  Authorization: `Basic ${Buffer.from(`kilo:${PASSWORD}`).toString("base64")}`,
};

function makeClient(authenticated) {
  return createKiloClient({
    baseUrl: BASE,
    headers: authenticated ? authHeaders : {},
    fetch: recordingFetch,
  });
}

const authed = makeClient(true);
const anon = makeClient(false);

const CALLS = {
  "auth.set": (c, a) => c.auth.set(a),
  "config.get": (c, a) => c.config.get(a),
  "config.overlay": (c, a) => c.config.overlay(a),
  "config.warnings": (c, a) => c.config.warnings(a),
  "global.dispose": (c, a) => c.global.dispose(a),
  "global.event": (c, a) => c.global.event(a),
  "global.health": (c, a) => c.global.health(a),
  "instance.dispose": (c, a) => c.instance.dispose(a),
  "instance.reload": (c, a) => c.instance.reload(a),
  "network.list": (c, a) => c.network.list(a),
  "permission.list": (c, a) => c.permission.list(a),
  "provider.list": (c, a) => c.provider.list(a),
  "pty.create": (c, a) => c.pty.create(a),
  "pty.remove": (c, a) => c.pty.remove(a),
  "question.list": (c, a) => c.question.list(a),
  "session.abort": (c, a) => c.session.abort(a),
  "session.create": (c, a) => c.session.create(a),
  "session.delete": (c, a) => c.session.delete(a),
  "session.deleteMessage": (c, a) => c.session.deleteMessage(a),
  "session.diff": (c, a) => c.session.diff(a),
  "session.fork": (c, a) => c.session.fork(a),
  "session.get": (c, a) => c.session.get(a),
  "session.list": (c, a) => c.session.list(a),
  "session.messages": (c, a) => c.session.messages(a),
  "session.prompt": (c, a) => c.session.prompt(a),
  "session.revert": (c, a) => c.session.revert(a),
  "session.status": (c, a) => c.session.status(a),
  "session.summarize": (c, a) => c.session.summarize(a),
  "session.unrevert": (c, a) => c.session.unrevert(a),
  "session.update": (c, a) => c.session.update(a),
};

function substitute(value, vars, used) {
  if (typeof value === "string" && value.startsWith("$")) {
    const key = value.slice(1);
    if (!(key in vars)) throw new Error(`no captured variable ${value}`);
    used.add(key);
    return vars[key];
  }
  if (Array.isArray(value)) return value.map((v) => substitute(v, vars, used));
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([k, v]) => [k, substitute(v, vars, used)]),
    );
  }
  return value;
}

function lookup(root, path) {
  let node = root;
  for (const segment of path.split(".")) {
    if (node === null || node === undefined) return undefined;
    node = node[segment];
  }
  return node;
}

async function runStep(client, step, vars, used) {
  const call = CALLS[step.call];
  if (!call) throw new Error(`no driver call mapped for ${step.call}`);
  const args = step.args === undefined ? {} : substitute(step.args, vars, used);
  if (step.call === "global.event") {
    const controller = new AbortController();
    const response = await client.global.event(args, {
      signal: controller.signal,
      sseMaxRetryAttempts: 0,
      onSseError: () => {},
    });
    const iterator = response.stream[Symbol.asyncIterator]();
    const events = [];
    const deadline = Date.now() + SSE_WINDOW_MS;
    while (Date.now() < deadline && events.length < 3) {
      const next = await Promise.race([
        iterator.next(),
        sleep(Math.max(0, deadline - Date.now())).then(() => null),
      ]);
      if (!next || next.done) break;
      events.push(next.value);
    }
    controller.abort();
    return { data: events, error: null };
  }
  const result = await call(client, args);
  return result ?? { data: null, error: null };
}

const groups = readdirSync(TRACES)
  .filter((name) => name.endsWith(".json"))
  .sort();
// The replay harness's per-process checkpoint-probe scratch path (a test
// fixture, never a wire input): registering it as a var lets golden
// normalization record `@string` instead of a run-specific path.
const vars = process.env.FAKTOR_COMPAT_PROBE_PATH
  ? { probePath: process.env.FAKTOR_COMPAT_PROBE_PATH }
  : {};
const traces = [];
for (const group of groups) {
  const parsed = JSON.parse(readFileSync(join(TRACES, group), "utf8"));
  for (const step of parsed.steps ?? []) {
    current = { requests: [], responses: [] };
    const used = new Set();
    let data = null;
    let error = null;
    try {
      const result = await Promise.race([
        runStep(step.unauthenticated ? anon : authed, step, vars, used),
        sleep(STEP_TIMEOUT_MS).then(() => {
          throw new Error(`step ${step.id} timed out after ${STEP_TIMEOUT_MS}ms`);
        }),
      ]);
      data = result.data ?? null;
      error = result.error ?? null;
    } catch (thrown) {
      error = {
        name: thrown?.name ?? "Error",
        message: String(thrown?.message ?? thrown),
      };
    }
    for (const [name, paths] of Object.entries(step.capture ?? {})) {
      for (const rawPath of paths) {
        const candidates = rawPath.startsWith("data.")
          ? [rawPath.slice("data.".length), rawPath]
          : [rawPath, `data.${rawPath}`];
        let found;
        for (const path of candidates) {
          found = lookup(data, path) ?? lookup(error, path);
          if (found !== undefined && found !== null) break;
        }
        if (found !== undefined && found !== null) {
          vars[name] = String(found);
          break;
        }
      }
    }
    traces.push({
      id: step.id,
      group: group.replace(/\.json$/, ""),
      request_count: current.requests.length,
      request: current.requests[0] ?? null,
      response: current.responses[0] ?? null,
      error,
      vars_used: [...used],
      vars: { ...vars },
    });
  }
}

writeFileSync(OUT, JSON.stringify({ steps: traces }, null, 2) + "\n");
process.exit(0);
