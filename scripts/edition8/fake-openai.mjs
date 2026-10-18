#!/usr/bin/env node
// Loopback-only fake OpenAI-compatible provider for the Edition 5 dogfood
// run. It exists so CI can exercise REAL daemon task admission/streaming
// without network dependencies. It never touches a real provider and is only
// reachable on 127.0.0.1.
//
// Usage: node fake-openai.mjs --port-file /tmp/port
// The server binds port 0 and writes the chosen port to the file.

import { createServer } from 'node:http';
import { writeFileSync } from 'node:fs';

function arg(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : null;
}

const portFile = arg('--port-file');
if (!portFile) {
  console.error('fake-openai: --port-file is required');
  process.exit(2);
}

const server = createServer(async (request, response) => {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  let body = {};
  try {
    body = chunks.length > 0 ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : {};
  } catch {
    response.writeHead(400, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'invalid json' } }));
    return;
  }

  if (request.method === 'GET' && request.url.startsWith('/v1/models')) {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(
      JSON.stringify({
        object: 'list',
        data: [{ id: 'm', object: 'model', owned_by: 'edition5-dogfood' }],
      }),
    );
    return;
  }

  if (request.method === 'POST' && request.url.startsWith('/v1/chat/completions')) {
    const text = 'edition5 dogfood ok';
    if (body.stream === true) {
      response.writeHead(200, {
        'content-type': 'text/event-stream',
        'cache-control': 'no-cache',
        connection: 'keep-alive',
      });
      const frames = [
        { choices: [{ index: 0, delta: { role: 'assistant' }, finish_reason: null }] },
        { choices: [{ index: 0, delta: { content: text }, finish_reason: null }] },
        {
          choices: [{ index: 0, delta: {}, finish_reason: 'stop' }],
          usage: { prompt_tokens: 12, completion_tokens: 7, total_tokens: 19 },
        },
      ];
      for (const frame of frames) {
        response.write(`data: ${JSON.stringify(frame)}\n\n`);
      }
      response.write('data: [DONE]\n\n');
      response.end();
      return;
    }
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(
      JSON.stringify({
        id: 'chatcmpl-edition5',
        object: 'chat.completion',
        model: body.model ?? 'm',
        choices: [{ index: 0, message: { role: 'assistant', content: text }, finish_reason: 'stop' }],
        usage: { prompt_tokens: 12, completion_tokens: 7, total_tokens: 19 },
      }),
    );
    return;
  }

  response.writeHead(404, { 'content-type': 'application/json' });
  response.end(JSON.stringify({ error: { message: 'not found' } }));
});

server.listen(0, '127.0.0.1', () => {
  const address = server.address();
  writeFileSync(portFile, String(address.port), 'utf8');
  console.log(`fake-openai listening on 127.0.0.1:${address.port}`);
});

for (const signal of ['SIGTERM', 'SIGINT']) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
