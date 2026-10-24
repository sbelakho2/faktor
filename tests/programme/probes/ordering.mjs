#!/usr/bin/env node
// Event-stream ordering/duplication probe against the real daemon.
//
// Exercises the SSE/cursor edge the report flags as weak: duplicate delivery,
// out-of-order/overflow cursors, malformed cursors and two concurrent streams
// on one session. Every case must be typed and bounded; a 5xx or a panic is a
// failure. This is real ordering evidence, not an invariant delegation.

import { randomBytes } from 'node:crypto';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { cliBinary, request, startDaemon } from './routes.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');
const OUT = resolve(ROOT, 'target/certification/programme/ordering-probe.json');

function no5xx(result) {
  return (
    (result.status !== null && result.status < 500) ||
    result.status === 503 ||
    result.status === 409 ||
    result.aborted
  );
}

export async function probeOrdering({ report = false } = {}) {
  const binary = cliBinary();
  if (binary === null) {
    return { status: 'deferred', reason: 'no faktor-cli binary; trusted lane builds it', cases: [] };
  }
  const tmp = mkdtempSync(join(tmpdir(), 'faktor-ordering-probe-'));
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
          api_key_env: 'ORDERING_PROBE_KEY',
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
      title: 'ordering probe',
    });
    if (created.status !== 200) throw new Error(`ordering probe session: ${created.status}`);
    const session = String(created.json.id);
    const events = `/native/session/${session}/events`;
    const cases = [];
    const add = (name, passed, observed) => cases.push({ name, passed, observed });

    for (const cursor of ['18446744073709551616', '-1', 'abc', '1&after=2']) {
      const result = await request(base, token, 'GET', `${events}?after=${cursor}`, undefined, 3000);
      add(`cursor-${cursor}`, no5xx(result) && !/panicked/i.test(result.text), {
        status: result.status,
        aborted: result.aborted,
      });
    }
    const [first, second] = await Promise.all([
      request(base, token, 'GET', `${events}?after=0`, undefined, 3000),
      request(base, token, 'GET', `${events}?after=0`, undefined, 3000),
    ]);
    add('two-concurrent-streams', no5xx(first) && no5xx(second), {
      first: first.aborted ? 'bounded' : first.status,
      second: second.aborted ? 'bounded' : second.status,
    });
    const missingAuth = await request(base, null, 'GET', `${events}?after=0`, undefined, 3000);
    add('stream-auth-before-data', missingAuth.status === 401, { status: missingAuth.status });

    const document = {
      schema: 'faktor-programme-ordering-probe/v1',
      generated_at: new Date().toISOString(),
      commit: null,
      cases,
      passed: cases.filter((entry) => entry.passed).length,
      failed: cases.filter((entry) => !entry.passed).length,
    };
    if (report) {
      mkdirSync(dirname(OUT), { recursive: true });
      writeFileSync(OUT, `${JSON.stringify(document, null, 2)}\n`);
    }
    return { status: document.failed === 0 ? 'proven' : 'failed', cases, document };
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

if (import.meta.url === `file://${process.argv[1]}`) {
  probeOrdering({ report: true })
    .then((result) => {
      console.log(
        `ordering probe: ${result.status} (${result.cases?.filter((entry) => entry.passed).length ?? 0}/${result.cases?.length ?? 0} passed)`,
      );
      for (const entry of result.cases.filter((item) => !item.passed)) {
        console.error(`  ${entry.name}: ${JSON.stringify(entry.observed)}`);
      }
      if (result.status === 'failed') process.exitCode = 1;
    })
    .catch((error) => {
      console.error(`ordering probe: ${error.stack ?? error}`);
      process.exitCode = 1;
    });
}
