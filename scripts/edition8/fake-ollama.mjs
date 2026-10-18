#!/usr/bin/env node
// Minimal Ollama-native loopback server for deterministic scale probes.
//
// Implements exactly the metadata + streaming endpoints the Ollama adapter
// probes:
//   GET  /api/tags   -> installed model list
//   POST /api/show   -> capabilities (tools/reasoning) + context length
//   GET  /api/ps     -> loaded model + allocated context
//   POST /api/chat   -> native NDJSON stream (content + done usage)
//
// The model advertises tools so the Implement-phase routing intent
// (tools + streaming, hard quality floor) is satisfiable. No OpenAI wire
// shape is involved.
//
// Usage: node scripts/edition8/fake-ollama.mjs [--port 0] [--port-file path]

import { createServer } from 'node:http';
import { writeFileSync } from 'node:fs';

const argv = process.argv.slice(2);
const value = (name, fallback) => {
  const at = argv.indexOf(`--${name}`);
  return at >= 0 && at + 1 < argv.length ? argv[at + 1] : fallback;
};
const portFile = value('port-file', null);
const requestedPort = Number(value('port', '0'));

const server = createServer((request, response) => {
  const url = new URL(request.url, 'http://127.0.0.1');
  const json = (status, body) => {
    const payload = JSON.stringify(body);
    response.writeHead(status, {
      'content-type': 'application/json',
      'content-length': Buffer.byteLength(payload),
    });
    response.end(payload);
  };
  if (request.method === 'GET' && url.pathname === '/api/tags') {
    json(200, { models: [{ name: 'm', model: 'm', size: 1, details: {} }] });
    return;
  }
  if (request.method === 'GET' && url.pathname === '/api/ps') {
    json(200, { models: [{ name: 'm', model: 'm', context_length: 65536, size_vram: 1 }] });
    return;
  }
  if (request.method === 'POST' && url.pathname === '/api/show') {
    json(200, {
      model_info: { 'general.architecture': 'probe', 'probe.context_length': 65536 },
      capabilities: ['completion', 'tools', 'thinking'],
      details: {},
    });
    return;
  }
  if (request.method === 'POST' && url.pathname === '/api/chat') {
    let body = '';
    request.on('data', (chunk) => {
      body += chunk;
      if (body.length > 4 * 1024 * 1024) request.destroy();
    });
    request.on('end', () => {
      let prompt = '';
      try {
        const parsed = JSON.parse(body);
        // The ACTIVE instruction is the LAST user message; using the whole
        // history would make every answer echo the first prompt.
        const users = (parsed.messages ?? []).filter((message) => message.role === 'user');
        prompt = users.length > 0 ? users[users.length - 1].content ?? '' : '';
      } catch {
        // A malformed body still gets a truthful deterministic answer.
      }
      const answer = `scale assistant answer for ${prompt.slice(0, 64)}`;
      response.writeHead(200, { 'content-type': 'application/x-ndjson' });
      response.write(
        `${JSON.stringify({
          model: 'm',
          message: { role: 'assistant', content: answer },
          done: false,
        })}\n`,
      );
      response.write(
        `${JSON.stringify({
          model: 'm',
          message: { role: 'assistant', content: '' },
          done: true,
          done_reason: 'stop',
          prompt_eval_count: Math.max(1, Math.ceil(prompt.length / 4)),
          eval_count: Math.max(1, Math.ceil(answer.length / 4)),
        })}\n`,
      );
      response.end();
    });
    return;
  }
  json(404, { error: 'not found' });
});

server.listen(requestedPort, '127.0.0.1', () => {
  const { port } = server.address();
  if (portFile !== null) writeFileSync(portFile, `${port}\n`);
  process.stdout.write(`fake-ollama listening on 127.0.0.1:${port}\n`);
});
