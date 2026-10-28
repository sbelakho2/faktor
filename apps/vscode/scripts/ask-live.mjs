// TEST-SEPARATION: oracle=external-fixture isolation=isolated-browser-profile, read-only-artifacts
// Live Ask harness: a real HTTP server + the real NativeClient + the real
// extension Ask helpers. It proves over the wire that Ask posts ONE ordinary
// prompt and reads conversation data only — no task run, contract, proof,
// verification, billing or control-plane request.

import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { loadConversation, sendAskTurn } from '../src/askFlow.ts';
import { NativeClient } from '../src/nativeClient.ts';
import { accountReadPlan, refreshReadPlan } from '../src/refreshReads.ts';

const requests = [];
const server = createServer((request, response) => {
  let body = '';
  request.on('data', (chunk) => {
    body += chunk;
  });
  request.on('end', () => {
    const url = new URL(request.url, 'http://127.0.0.1');
    requests.push({ method: request.method, path: url.pathname, query: url.search, body });
    response.setHeader('content-type', 'application/json');
    if (request.method === 'POST' && url.pathname === '/native/session/7/prompt') {
      response.end(JSON.stringify({ op_id: 'op-1', run_id: 'run-1', accepted: true, queued: false }));
      return;
    }
    if (url.pathname === '/native/messages') {
      response.end(
        JSON.stringify({
          sessionId: '7',
          messages: [
            {
              seq: 1,
              id: 1,
              role: 'assistant',
              createdMs: 1_700_000_000_000,
              data: { text: 'Hello! How can I help?' },
              parts: [],
            },
          ],
          hasMore: false,
          nextBefore: null,
        }),
      );
      return;
    }
    if (url.pathname === '/native/sessions') {
      response.end(
        JSON.stringify({
          sessions: [
            { id: '7', title: 'hello', provider: 'fake', model: 'm', state: 'idle' },
          ],
        }),
      );
      return;
    }
    if (url.pathname === '/native/permissions') {
      response.end(JSON.stringify({ permissions: [] }));
      return;
    }
    // Any other route is a product-boundary violation for this harness.
    response.statusCode = 404;
    response.end(JSON.stringify({ error: 'not_found', path: url.pathname }));
  });
});

await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
const { port } = server.address();

let passed = 0;
async function test(label, fn) {
  try {
    await fn();
    passed += 1;
    console.log(`PASS  ${label}`);
  } catch (error) {
    console.error(`FAILED  ${label} — ${error && error.message ? error.message : String(error)}`);
    process.exitCode = 1;
  }
}

try {
  const client = new NativeClient({
    baseUrl: `http://127.0.0.1:${port}`,
    bearerToken: 'test-password',
    timeoutMs: 5_000,
  });

  await test('Ask posts one plain prompt and reads conversation data only', async () => {
    requests.length = 0;
    const receipt = await sendAskTurn(client, {
      sessionId: '7',
      prompt: 'hello',
      files: ['README.md'],
      submissionId: '11111111-1111-4111-8111-111111111111',
    });
    const conversation = await loadConversation(client, '7', 50);
    assert.equal(receipt.accepted, true);
    assert.equal(receipt.run_id, 'run-1');
    assert.equal(conversation.messages.sessionId, '7');
    assert.equal(conversation.sessions.length, 1);
    assert.deepEqual(conversation.permissions, []);
    const paths = requests.map((entry) => `${entry.method} ${entry.path}`);
    assert.deepEqual(paths, [
      'POST /native/session/7/prompt',
      'GET /native/messages',
      'GET /native/sessions',
      'GET /native/permissions',
    ]);
    const forbidden = [
      '/task',
      '/task-run',
      '/proof',
      '/verification',
      '/usage',
      '/billing',
      '/entitlement',
      '/identity',
      '/account',
      '/subscription',
      '/control',
    ];
    for (const request of requests) {
      for (const prefix of forbidden) {
        assert.ok(
          !request.path.includes(prefix),
          `Ask must not request ${prefix}: ${request.method} ${request.path}`,
        );
      }
    }
  });

  await test('the Ask prompt body has no task/contract/proof member', async () => {
    const prompt = requests.find((entry) => entry.path === '/native/session/7/prompt');
    assert.ok(prompt, 'the prompt request was observed');
    const body = JSON.parse(prompt.body);
    assert.deepEqual(Object.keys(body).sort(), ['files', 'prompt', 'session_id', 'submission_id']);
    for (const key of [
      'completionContract',
      'completion_contract',
      'contractRows',
      'contract_rows',
      'budgetTokens',
      'budget_tokens',
      'budgetCostMicro',
      'engineering_contract',
      'proofObligations',
      'task',
    ]) {
      assert.ok(!(key in body), `Ask prompt must not carry ${key}`);
    }
  });

  await test('no billing request ever occurs during Ask or run refresh', () => {
    for (const askOnly of [true, false]) {
      const plan = refreshReadPlan({ askOnly });
      assert.ok(!plan.includes('account'));
      assert.ok(!plan.includes('subscription'));
    }
    assert.deepEqual(accountReadPlan(), ['account']);
    assert.ok(
      !requests.some((entry) => /billing|usage|entitlement|identity/i.test(entry.path)),
      'no commercial route was hit by the Ask flow',
    );
  });

  await test('Task-mode refresh still reads the run/task/proof projection', () => {
    const task = refreshReadPlan({ askOnly: false });
    for (const read of ['tasks', 'taskRuns', 'verification', 'proof']) {
      assert.ok(task.includes(read), `Task refresh must include ${read}`);
    }
  });
} finally {
  await new Promise((resolve) => server.close(resolve));
}

if (process.exitCode === 1) {
  console.error('FAILED');
} else {
  console.log(`\n${passed} live Ask checks passed`);
}
