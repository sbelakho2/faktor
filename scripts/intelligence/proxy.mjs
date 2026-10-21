#!/usr/bin/env node
// Bounded OpenAI/Ollama wire proxy used only by the intelligence measurement.
//
// It forwards to the real Ollama runtime, filters the live model list to the
// model under test (so Economy routing cannot silently pick another installed
// model), and records the exact provider usage frames the model returned.
// It is instrumentation: it never rewrites a request or a response body.

import fs from "node:fs";
import http from "node:http";

const UPSTREAM = process.env.OLLAMA_UPSTREAM || "http://127.0.0.1:11434";
const PORT = Number(process.env.PROXY_PORT || 11500);
const LOG = process.env.PROXY_LOG || "/tmp/faktor-intelligence-proxy.jsonl";
const ONLY = process.env.PROXY_ONLY_MODEL || "qwen3.8:latest";

fs.writeFileSync(LOG, "");

function log(row) {
  fs.appendFileSync(LOG, JSON.stringify(row) + "\n");
}

const server = http.createServer(async (req, res) => {
  const started = Date.now();
  const url = new URL(req.url, `http://127.0.0.1:${PORT}`);
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const body = Buffer.concat(chunks);

  if (url.pathname === "/api/tags") {
    const upstream = await fetch(UPSTREAM + url.pathname);
    const json = await upstream.json();
    json.models = (json.models || []).filter(
      (model) => model.name === ONLY || model.model === ONLY
    );
    const out = Buffer.from(JSON.stringify(json));
    res.writeHead(upstream.status, {
      "content-type": "application/json",
      "content-length": out.length,
    });
    res.end(out);
    return;
  }

  let parsed = null;
  try {
    parsed = body.length ? JSON.parse(body.toString("utf8")) : null;
  } catch {}

  const headers = { ...req.headers };
  delete headers.host;
  delete headers["content-length"];
  let upstream;
  try {
    upstream = await fetch(UPSTREAM + url.pathname + url.search, {
      method: req.method,
      headers,
      body: body.length ? body : undefined,
    });
  } catch (error) {
    const message = Buffer.from(JSON.stringify({ error: String(error) }));
    res.writeHead(502, { "content-type": "application/json", "content-length": message.length });
    res.end(message);
    log({ path: url.pathname, model: parsed?.model || null, status: 502, error: String(error) });
    return;
  }

  const row = {
    path: url.pathname,
    model: parsed?.model || null,
    stream: parsed?.stream ?? null,
    tool_calls: 0,
    prompt_eval_count: null,
    prompt_eval_cached_count: null,
    eval_count: null,
    ms: null,
    status: upstream.status,
  };

  if (url.pathname === "/api/chat" && parsed?.stream === false) {
    const out = Buffer.from(await upstream.arrayBuffer());
    try {
      const frame = JSON.parse(out.toString("utf8"));
      row.prompt_eval_count = frame.prompt_eval_count ?? null;
      row.prompt_eval_cached_count = frame.prompt_eval_cached_count ?? null;
      row.eval_count = frame.eval_count ?? null;
      row.tool_calls = Array.isArray(frame.message?.tool_calls)
        ? frame.message.tool_calls.length
        : 0;
    } catch {}
    res.writeHead(upstream.status, {
      "content-type": "application/json",
      "content-length": out.length,
    });
    res.end(out);
    row.ms = Date.now() - started;
    log(row);
    return;
  }

  if (url.pathname === "/api/chat") {
    if (process.env.PROXY_BODY_DIR) {
      fs.mkdirSync(process.env.PROXY_BODY_DIR, { recursive: true });
      const name = String(Date.now()) + "-" + Math.random().toString(16).slice(2) + ".json";
      fs.writeFileSync(
        process.env.PROXY_BODY_DIR + "/" + name,
        JSON.stringify(parsed, null, 2)
      );
    }
    res.writeHead(upstream.status, {
      "content-type": upstream.headers.get("content-type") || "application/x-ndjson",
      "transfer-encoding": "chunked",
    });
    const reader = upstream.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      res.write(Buffer.from(value));
      buffer += decoder.decode(value, { stream: true });
      let index;
      while ((index = buffer.indexOf("\n")) >= 0) {
        const line = buffer.slice(0, index).trim();
        buffer = buffer.slice(index + 1);
        if (!line) continue;
        try {
          const frame = JSON.parse(line);
          if (frame.prompt_eval_count != null) row.prompt_eval_count = frame.prompt_eval_count;
          if (frame.prompt_eval_cached_count != null) {
            row.prompt_eval_cached_count = frame.prompt_eval_cached_count;
          }
          if (frame.eval_count != null) row.eval_count = frame.eval_count;
          const message = frame.message || frame;
          if (Array.isArray(message?.tool_calls)) {
            row.tool_calls += message.tool_calls.length;
          }
        } catch {}
      }
    }
    res.end();
    row.ms = Date.now() - started;
    log(row);
    return;
  }

  const out = Buffer.from(await upstream.arrayBuffer());
  res.writeHead(upstream.status, {
    "content-type": upstream.headers.get("content-type") || "application/json",
    "content-length": out.length,
  });
  res.end(out);
  row.ms = Date.now() - started;
  log(row);
});

process.on("unhandledRejection", (error) => {
  console.error("intelligence proxy rejection:", error);
});

server.listen(PORT, "127.0.0.1", () => {
  console.log(`intelligence proxy on ${PORT} -> ${UPSTREAM} (only ${ONLY})`);
});
