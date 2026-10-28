#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=isolated-browser-profile, read-only-artifacts
// LIVE Ask evidence over the real installed extension host modules:
//   * start the verified Faktor daemon from the live install (DeepSeek config
//     comes from the live data dir's faktor-plus.json + provider_env_file),
//   * create a real durable session,
//   * stop the daemon and start a new one on a NEW loopback port,
//   * prove the old client fails with the typed NativeTransportError,
//   * run sendAskTurnWithRecovery: the stale address is replaced and the ONE
//     retry posts a real DeepSeek prompt,
//   * wait for the durable assistant message and record it.
//
// The provider key is read by the daemon from FAKTOR_DEEPSEEK_API_KEY via the
// configured provider_env_file. This script never reads or prints it.
//
// Usage: node scripts/live-ask-evidence.mjs [out-dir]
// Env: FAKTOR_LIVE_HOME (default ~/.faktor-live), FAKTOR_LIVE_TIMEOUT_MS.

import { createRequire } from 'node:module';
import { randomUUID } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..', '..');
const outDir = resolve(process.argv[2] || join(repoRoot, 'docs/specs/live-evidence/ask-fetch-fix'));
const liveHome = process.env.FAKTOR_LIVE_HOME || join(homedir(), '.faktor-live');
const binary = join(liveHome, 'bin', 'faktor');
const configSource = join(liveHome, 'data', 'faktor-plus.json');
const timeoutMs = Number(process.env.FAKTOR_LIVE_TIMEOUT_MS || '180000');

const require = createRequire(import.meta.url);
const { startDaemon, stopDaemon } = require(join(here, '..', 'out', 'daemon.js'));
const { NativeClient, NativeTransportError } = require(join(here, '..', 'out', 'nativeClient.js'));
const { sendAskTurnWithRecovery, isTransportFailure } = require(
  join(here, '..', 'out', 'askRecovery.js'),
);

function assert(condition, message) {
  if (!condition) {
    throw new Error(message);
  }
}

if (!existsSync(binary)) {
  throw new Error(`missing live daemon executable: ${binary}`);
}
if (!existsSync(configSource)) {
  throw new Error(`missing live provider config: ${configSource}`);
}
assert(existsSync(`${binary}.sha256`), `missing checksum record for ${binary}`);

const dataDir = mkdtempSync(join(tmpdir(), 'faktor-live-ask-'));
copyFileSync(configSource, join(dataDir, 'faktor-plus.json'));
// A stable throwaway workspace keeps the daemon's shadow-copy admission from
// racing the actively edited checkout this script lives in.
const workspaceRoot = mkdtempSync(join(tmpdir(), 'faktor-live-ask-workspace-'));
writeFileSync(join(workspaceRoot, 'README.md'), '# Live Ask evidence workspace\n');

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

async function waitForAssistant(client, sessionId, deadline) {
  let last = [];
  while (Date.now() < deadline) {
    const page = await client.messages(sessionId, { limit: 50 });
    last = page.messages;
    for (const assistant of [...last].reverse()) {
      if (assistant.role !== 'assistant') {
        continue;
      }
      const parts = Array.isArray(assistant.parts) ? assistant.parts : [];
      const text = parts
        .filter((part) => part && part.kind === 'text' && part.data && typeof part.data.text === 'string')
        .map((part) => part.data.text)
        .join('\n')
        .trim();
      if (text.length > 0) {
        return { message: assistant, text };
      }
    }
    await sleep(2000);
  }
  throw new Error(
    `no assistant message within the deadline; last roles: ${last
      .map((message) => message.role)
      .join(',')}`,
  );
}

let first = null;
let second = null;
const evidence = {
  schema: 'faktor-live-ask-evidence/v1',
  capturedAt: new Date().toISOString(),
  repoRoot,
  liveHome,
  dataDir,
  binary,
  provider: 'deepseek',
  model: 'deepseek-chat',
  ok: false,
  steps: {},
};

try {
  mkdirSync(outDir, { recursive: true });
  first = await startDaemon({
    workspaceRoot,
    binaryPath: binary,
    dataDir,
    installRoot: join(liveHome, 'bin'),
    startupTimeoutMs: 120000,
  });
  const oldClient = new NativeClient({
    baseUrl: first.baseUrl,
    bearerToken: first.bearerToken,
    timeoutMs,
  });
  await oldClient.readyOrThrow(120000);
  const session = await oldClient.createSession({
    provider: 'deepseek',
    model: 'deepseek-chat',
    workspace: workspaceRoot,
    title: 'Ask live evidence',
  });
  evidence.steps.start = {
    oldBaseUrl: first.baseUrl,
    sessionId: session.id,
    ready: true,
  };

  await stopDaemon(first);
  first = null;
  second = await startDaemon({
    workspaceRoot,
    binaryPath: binary,
    dataDir,
    installRoot: join(liveHome, 'bin'),
    startupTimeoutMs: 120000,
  });
  assert(second.baseUrl !== evidence.steps.start.oldBaseUrl, 'the restart must use a new port');
  const newClient = new NativeClient({
    baseUrl: second.baseUrl,
    bearerToken: second.bearerToken,
    timeoutMs,
  });
  await newClient.readyOrThrow(120000);

  const typedFailure = await oldClient.health().then(
    () => null,
    (error) => error,
  );
  assert(typedFailure !== null, 'the stale client must fail against the old port');
  assert(
    typedFailure instanceof NativeTransportError,
    `the stale client must fail typed, got ${typedFailure && typedFailure.name}: ${typedFailure && typedFailure.message}`,
  );
  assert(isTransportFailure(typedFailure), 'the failure must classify as transport');
  evidence.steps.staleTransport = {
    newBaseUrl: second.baseUrl,
    errorName: typedFailure.name,
    errorCode: typedFailure.code,
    message: typedFailure.message,
    retryable: typedFailure.retryable,
  };

  let recoveredWith = null;
  const outcome = await sendAskTurnWithRecovery(
    {
      ensureConnection: async () => ({ client: oldClient, sessionId: session.id }),
      recoverTransport: async (connection) => {
        recoveredWith = connection;
        return { client: newClient, sessionId: connection.sessionId };
      },
      rebindSession: async () => {
        throw new Error('the live stale-port path must reconnect, not rebind');
      },
    },
    {
      prompt: 'Reply with exactly: ASK-LIVE-OK',
      files: [],
      submissionId: randomUUID(),
    },
  );
  assert(recoveredWith !== null, 'the stale connection was recovered');
  assert(outcome.attempts === 2, `expected one retry, got ${outcome.attempts}`);
  evidence.steps.prompt = {
    attempts: outcome.attempts,
    recovered: outcome.recovered,
    receipt: outcome.receipt,
    sessionId: outcome.connection.sessionId,
  };

  const assistant = await waitForAssistant(newClient, session.id, Date.now() + timeoutMs);
  evidence.steps.assistant = {
    messageId: assistant.message.id,
    seq: assistant.message.seq,
    role: assistant.message.role,
    text: assistant.text.slice(0, 2000),
  };
  assert(
    assistant.text.toUpperCase().includes('ASK-LIVE-OK'),
    `the assistant reply did not carry the marker: ${assistant.text.slice(0, 200)}`,
  );
  evidence.ok = true;
} catch (error) {
  evidence.error = {
    name: error && error.name ? error.name : 'Error',
    message: error && error.message ? error.message : String(error),
  };
  throw error;
} finally {
  if (first !== null) {
    try {
      await stopDaemon(first);
    } catch {
      /* teardown is best-effort in evidence capture */
    }
  }
  if (second !== null) {
    try {
      await stopDaemon(second);
    } catch {
      /* teardown is best-effort in evidence capture */
    }
  }
  mkdirSync(outDir, { recursive: true });
  writeFileSync(join(outDir, 'live-ask.json'), `${JSON.stringify(evidence, null, 2)}\n`);
  rmSync(dataDir, { recursive: true, force: true });
  rmSync(workspaceRoot, { recursive: true, force: true });
}

console.log(JSON.stringify({ ok: evidence.ok, out: join(outDir, 'live-ask.json') }));
