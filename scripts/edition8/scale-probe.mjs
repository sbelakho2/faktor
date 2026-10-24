#!/usr/bin/env node
// Edition 8 scale probe: new limits, real daemon and real host models.
//
// Drives the REAL daemon over its HTTP/native routes through limits that no
// earlier gate exercised:
//   * 1,000 real prompt turns, each followed by a forced durable compaction
//   * 100,000-message transcript paging + search (message.data AND part rows)
//   * 5,000 managed memory facts with edits, deletes, retention and scope
//   * 2,000 History sessions reached page by page
//   * 500 child-agent rows in one run projection
//   * 10,000 proof/claim rows in one verification record
//   * 500 MCP servers, 500 skills, 500 hooks
//   * 16 concurrent sessions with live SSE streams
//
// Every probe records per-operation latencies + RSS, and fails on an
// unbounded read, a wrong count, a lost/duplicated row, a stale checkpoint or
// an O(n^2)-shaped latency curve. Fixture rows are written BEFORE the daemon
// starts through the same store APIs (crates/cli/examples/e8_scale_seed.rs);
// the daemon serves them over real routes. No route is stubbed.
//
// Usage:
//   node scripts/edition8/scale-probe.mjs --turns 1000
//   node scripts/edition8/scale-probe.mjs --quick        # small smoke
//
// Output: target/certification/edition8/scale.raw.json for the full default
// probe only; --quick/--skip-turns/--direct-turns and non-report parameter
// values write `scale.<mode>.raw.json` and are never gate evidence.

import { spawn, spawnSync } from 'node:child_process';
import { createHash, randomBytes, randomUUID } from 'node:crypto';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const argv = process.argv.slice(2);
const arg = (name, fallback = null) => {
  const index = argv.indexOf(`--${name}`);
  return index >= 0 && index + 1 < argv.length ? argv[index + 1] : fallback;
};
const QUICK = argv.includes('--quick');
const TURNS = Number(arg('turns', QUICK ? '20' : '1000'));
const MESSAGES = Number(arg('messages', QUICK ? '2000' : '100000'));
const SESSIONS = Number(arg('sessions', QUICK ? '300' : '2000'));
const CHILDREN = Number(arg('children', QUICK ? '80' : '500'));
const CLAIMS = Number(arg('claims', QUICK ? '800' : '10000'));
const MEMORY_FACTS = Number(arg('memory', QUICK ? '500' : '5000'));
const STREAM_SESSIONS = 16;
const SKIP_TURNS = argv.includes('--skip-turns');
const DIRECT_TURNS = argv.includes('--direct-turns');
const RUN_ID = randomBytes(6).toString('hex');
// The canonical gate artifact is reserved for the report's full scheduled
// probe (real prompt turns, 1,000/100,000/5,000/2,000/500/10,000/16).
// Reduced/diagnostic runs write a distinct file so they can never poison a
// later `run.mjs` conversion with under-counted evidence.
const FULL_COVERAGE =
  !QUICK &&
  !SKIP_TURNS &&
  !DIRECT_TURNS &&
  TURNS === 1000 &&
  MESSAGES === 100000 &&
  MEMORY_FACTS === 5000 &&
  SESSIONS === 2000 &&
  CHILDREN === 500 &&
  CLAIMS === 10000;
const MODE = QUICK ? 'quick' : SKIP_TURNS ? 'skip-turns' : DIRECT_TURNS ? 'direct-turns' : 'reduced';
const OUT_NAME = FULL_COVERAGE ? 'scale.raw.json' : `scale.${MODE}.raw.json`;

function sha(bytes) {
  return `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
}

function percentile(values, fraction) {
  if (values.length === 0) return 0;
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * fraction))];
}

function summary(values) {
  // One sort serves p50/p95/p99: the historical helper copied+sorted the
  // same sample set three times per probe (12 calls per full run).
  const sorted = values.length === 0 ? [] : [...values].sort((a, b) => a - b);
  const at = (fraction) =>
    sorted.length === 0
      ? 0
      : sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * fraction))];
  return {
    n: values.length,
    p50: Number(at(0.5).toFixed(3)),
    p95: Number(at(0.95).toFixed(3)),
    p99: Number(at(0.99).toFixed(3)),
    max: Number(Math.max(...values, 0).toFixed(3)),
  };
}

function rssKb(pid) {
  // Linux (the nightly lane): read the kernel's own counter, no procps.
  try {
    const status = readFileSync(`/proc/${pid}/status`, 'utf8');
    const match = /^VmRSS:\s+(\d+)\s+kB$/m.exec(status);
    if (match) return Number(match[1]);
  } catch {
    // fall through to ps on hosts without procfs
  }
  try {
    const out = spawnSync('ps', ['-o', 'rss=', '-p', String(pid)], { encoding: 'utf8' }).stdout;
    const value = Number(String(out).trim().split(/\s+/)[0]);
    return Number.isFinite(value) ? value : null;
  } catch {
    return null;
  }
}

// A real project workspace is a git worktree. Shadowed task runs build a
// canonical tree manifest from the workspace root; an empty non-repo makes
// the shadow watch settle against a tree it cannot re-read. Seed one file
// and one commit so the scale turns exercise the same shape as the dogfood.
function initWorkspace(dir) {
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'README.md'), '# Edition 8 scale workspace\n');
  const git = (args) => spawnSync('git', args, { cwd: dir, encoding: 'utf8' });
  git(['init', '-q']);
  git(['config', 'user.email', 'scale@faktor.invalid']);
  git(['config', 'user.name', 'Faktor Scale']);
  git(['add', '.']);
  git(['commit', '-q', '-m', 'scale workspace']);
}

const failures = [];
const probes = [];
function fail(probe, message) {
  failures.push(`${probe}: ${message}`);
  throw new Error(`${probe}: ${message}`);
}
function assert(probe, condition, message) {
  if (!condition) fail(probe, message);
}
async function probe(id, fn) {
  const started = process.hrtime.bigint();
  try {
    const result = await fn();
    probes.push({
      id,
      passed: true,
      ms: Number((Number(process.hrtime.bigint() - started) / 1e6).toFixed(1)),
      ...result,
    });
    console.log(`[scale] ${id} passed (${((Number(process.hrtime.bigint() - started)) / 1e9).toFixed(1)}s)`);
    return result;
  } catch (error) {
    probes.push({
      id,
      passed: false,
      ms: Number((Number(process.hrtime.bigint() - started) / 1e6).toFixed(1)),
      error: error && error.message ? error.message : String(error),
    });
    throw error;
  }
}

function storeBinary() {
  const candidates = [
    process.env.FAKTOR_CLI_BIN,
    resolve(ROOT, 'target/release/faktor-cli'),
    resolve(ROOT, 'target/debug/faktor-cli'),
  ].filter((value) => typeof value === 'string' && value.length > 0);
  const binary = candidates.find((candidate) => existsSync(candidate));
  if (!binary) throw new Error('no faktor-cli binary; build one or set FAKTOR_CLI_BIN');
  return binary;
}

function appendBinary() {
  const candidates = [
    process.env.FAKTOR_E8_APPEND_TURN,
    resolve(ROOT, 'target/release/examples/e8_append_turn'),
    resolve(ROOT, 'target/debug/examples/e8_append_turn'),
  ];
  const binary = candidates.find((candidate) => existsSync(candidate));
  if (!binary) throw new Error('e8_append_turn example is not built');
  return binary;
}

function seedBinary() {
  const candidates = [
    process.env.FAKTOR_E8_SCALE_SEED,
    resolve(ROOT, 'target/release/examples/e8_scale_seed'),
    resolve(ROOT, 'target/debug/examples/e8_scale_seed'),
  ];
  const binary = candidates.find((candidate) => existsSync(candidate));
  if (binary) return binary;
  const build = spawnSync(
    'cargo',
    ['build', '-q', '-p', 'faktor-cli', '--example', 'e8_scale_seed'],
    { cwd: ROOT, encoding: 'utf8' },
  );
  if (build.status !== 0) {
    throw new Error(`e8_scale_seed build failed: ${build.stderr?.slice(-2000) ?? ''}`);
  }
  const built = resolve(ROOT, 'target/debug/examples/e8_scale_seed');
  if (!existsSync(built)) throw new Error('e8_scale_seed binary missing after build');
  return built;
}

async function postJson(base, token, path, body) {
  const response = await fetch(`${base}${path}`, {
    method: 'POST',
    headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  const text = await response.text();
  let json = null;
  try {
    json = text.length > 0 ? JSON.parse(text) : null;
  } catch {
    json = null;
  }
  return { status: response.status, json, text };
}

async function waitForAgentsIdle(base, token, sessionId, deadlineMs = 20_000) {
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    const agents = await getJson(base, token, `/native/agents?session=${sessionId}`);
    if (agents.status === 200 && Array.isArray(agents.json) && agents.json.length > 0) {
      const running = agents.json.some((entry) => entry.state === 'Running' || entry.state === 'Starting');
      if (!running) return true;
    }
    await new Promise((done) => setTimeout(done, 25));
  }
  return false;
}

async function waitForTurn(collector, fromIndex, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (collector.error !== null) throw new Error(`SSE failed: ${collector.error}`);
    const fresh = collector.events.slice(fromIndex);
    const failed = fresh.find((event) => event.event === 'failed' || event.event === 'turn_failed');
    if (failed !== undefined) {
      const trail = fresh.slice(-8).map((event) => `${event.event}:${String(event.data).slice(0, 300)}`).join(' | ');
      throw new Error(`turn failed: ${String(failed.data).slice(0, 800)}; trail=${trail}`);
    }
    if (fresh.some((event) => event.event === 'turn_completed')) return;
    await new Promise((done) => setTimeout(done, 25));
  }
  const seen = collector.events.slice(fromIndex).map((event) => event.event).join(',');
  throw new Error(`turn did not complete within ${timeoutMs}ms (${seen || 'no events'})`);
}

async function waitSessionIdle(base, token, sessionId, deadlineMs = 20_000) {
  const terminal = new Set(['Done', 'Completed', 'Cancelled', 'Failed']);
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    const agents = await getJson(base, token, `/native/agents?session=${sessionId}`);
    const rows = Array.isArray(agents.json) ? agents.json : [];
    const runs = await getJson(base, token, `/native/session/${sessionId}/task-runs`);
    const runRows = Array.isArray(runs.json) ? runs.json : [];
    const active = rows.filter((row) => !terminal.has(row.state));
    const activeRuns = runRows.filter((row) => !terminal.has(row.state));
    if (active.length === 0 && activeRuns.length === 0) return true;
    await new Promise((done) => setTimeout(done, 100));
  }
  return false;
}

async function settleSessionRuns(base, token, sessionId, deadlineMs = 20_000) {
  const terminal = new Set(['Done', 'Completed', 'Cancelled', 'Failed']);
  const deadline = Date.now() + deadlineMs;
  while (Date.now() < deadline) {
    const agents = await getJson(base, token, `/native/agents?session=${sessionId}`);
    const rows = Array.isArray(agents.json) ? agents.json : [];
    const active = rows.filter((row) => !terminal.has(row.state));
    // The live shadow is owned by a durable task run; the agent projection
    // can settle before the run row does. Cancel both views so the next
    // shadowed task does not answer the typed "interrupted run" 409.
    const runs = await getJson(base, token, `/native/session/${sessionId}/task-runs`);
    const runRows = Array.isArray(runs.json) ? runs.json : [];
    const activeRuns = runRows.filter((row) => !terminal.has(row.state));
    if ((rows.length > 0 || runRows.length > 0) && active.length === 0 && activeRuns.length === 0) {
      return true;
    }
    for (const row of active) {
      await postJson(
        base,
        token,
        `/native/session/${sessionId}/task-runs/${row.agent_id}/cancel`,
        {},
      );
    }
    for (const row of activeRuns) {
      await postJson(
        base,
        token,
        `/native/session/${sessionId}/task-runs/${row.id}/cancel`,
        {},
      );
    }
    await new Promise((done) => setTimeout(done, 50));
  }
  return false;
}

async function getJson(base, token, path) {
  const response = await fetch(`${base}${path}`, {
    headers: { authorization: `Bearer ${token}` },
  });
  const text = await response.text();
  let json = null;
  try {
    json = text.length > 0 ? JSON.parse(text) : null;
  } catch {
    json = null;
  }
  return { status: response.status, json, text };
}

async function startDaemon(store, dataDir, config, token, logPath) {
  const child = spawn(store, ['serve', '--port', '0', '--data-dir', dataDir, '--config', config], {
    cwd: ROOT,
    env: { ...process.env, FAKTOR_SERVER_PASSWORD: token },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  const collect = (chunk) => {
    output += chunk.toString();
    writeFileSync(logPath, output);
  };
  child.stdout.on('data', collect);
  child.stderr.on('data', collect);
  const deadline = Date.now() + 30_000;
  let base = null;
  while (Date.now() < deadline) {
    const match = /(http:\/\/127\.0\.0\.1:\d+)/.exec(output);
    if (match !== null) {
      base = match[1];
      break;
    }
    if (child.exitCode !== null) break;
    await new Promise((done) => setTimeout(done, 40));
  }
  if (base === null) throw new Error(`daemon did not start: ${output.slice(-1200)}`);
  while (Date.now() < deadline) {
    try {
      const ready = await fetch(`${base}/native/ready`, {
        headers: { authorization: `Bearer ${token}` },
      });
      if (ready.ok) return { child, base };
    } catch {
      // loopback poll
    }
    await new Promise((done) => setTimeout(done, 50));
  }
  throw new Error(`daemon never became ready: ${output.slice(-1200)}`);
}

async function stop(child) {
  if (!child || child.exitCode !== null) return;
  child.kill('SIGKILL');
  await new Promise((done) => {
    if (child.exitCode !== null) done();
    else child.once('exit', done);
  });
}

function sseCollector(base, token, sessionId) {
  const controller = new AbortController();
  const state = {
    controller,
    ids: [],
    events: [],
    closed: false,
    error: null,
    done: null,
  };
  state.done = (async () => {
    try {
      const response = await fetch(`${base}/native/session/${sessionId}/events?after=0`, {
        headers: { authorization: `Bearer ${token}`, accept: 'text/event-stream' },
        signal: controller.signal,
      });
      if (!response.ok) throw new Error(`SSE ${response.status}`);
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        buffer += decoder.decode(value, { stream: true });
        let boundary = buffer.indexOf('\n\n');
        while (boundary >= 0) {
          const frame = buffer.slice(0, boundary);
          buffer = buffer.slice(boundary + 2);
          const lines = frame.split('\n');
          const id = lines.find((line) => line.startsWith('id: '))?.slice(4);
          const event = lines.find((line) => line.startsWith('event: '))?.slice(7);
          const data = lines.find((line) => line.startsWith('data: '))?.slice(6);
          if (id !== undefined && event !== 'heartbeat') {
            state.ids.push(Number(id));
            state.events.push({ id: Number(id), event, data });
          }
          boundary = buffer.indexOf('\n\n');
        }
      }
    } catch (error) {
      if (!state.closed) state.error = error?.message ?? String(error);
    }
  })();
  return state;
}

async function main() {
  const store = storeBinary();
  const seed = seedBinary();
  const appendTurn = appendBinary();
  const tmp = mkdtempSync(join(tmpdir(), 'faktor-e8-scale-'));
  const data = join(tmp, 'data');
  const config = join(tmp, 'faktor.json');
  const log = join(tmp, 'daemon.log');
  const token = randomBytes(32).toString('hex');
  const fakePort = join(tmp, 'fake.port');
  const fake = spawn(process.execPath, [resolve(ROOT, 'scripts/edition8/fake-ollama.mjs'), '--port-file', fakePort], {
    cwd: ROOT,
    stdio: ['ignore', 'ignore', 'ignore'],
  });
  let daemon = null;
  const samples = [];
  let sampler = null;
  try {
    for (let attempt = 0; attempt < 100 && !existsSync(fakePort); attempt += 1) {
      await new Promise((done) => setTimeout(done, 30));
    }
    if (!existsSync(fakePort)) throw new Error('fake provider did not bind');
    const port = readFileSync(fakePort, 'utf8').trim();
    writeFileSync(
      config,
      `${JSON.stringify({
        config_version: 1,
        model: 'm',
        sandbox: { network: [`http://127.0.0.1:${port}`] },
        providers: [
          {
            kind: 'ollama',
            id: 'p1',
            base_url: `http://127.0.0.1:${port}`,
            allow_loopback: true,
            quality: {
              coding_reliability: 95,
              context_reliability: 95,
              tool_reliability: 95,
              reasoning_reliability: 95,
            },
          },
        ],
      })}\n`,
    );
    const seeded = spawnSync(
      seed,
      [
        '--data-dir',
        data,
        '--messages',
        String(MESSAGES),
        '--sessions',
        String(SESSIONS),
        '--children',
        String(CHILDREN),
        '--claims',
        String(CLAIMS),
      ],
      { encoding: 'utf8' },
    );
    if (seeded.status !== 0) throw new Error(`seed failed: ${seeded.stderr.slice(-2000)}`);
    const ids = JSON.parse(seeded.stdout.trim().split('\n').pop());
    daemon = await startDaemon(store, data, config, token, log);
    const { base } = daemon;
    const startedRss = rssKb(daemon.child.pid);
    sampler = setInterval(() => samples.push(rssKb(daemon.child.pid)), 250);

    // ---------------------------------------------------------------- turns
    if (!SKIP_TURNS) {
      await probe('turns-compaction', async () => {
        const turnWorkspace = join(tmp, 'turn-workspace');
        initWorkspace(turnWorkspace);
        const created = await postJson(base, token, '/native/session', {
          provider: 'p1',
          model: 'm',
          workspace: turnWorkspace,
          title: `scale ${RUN_ID} turns`,
        });
        assert('turns-compaction', created.status === 200, `create session ${created.status}`);
        const sessionId = String(created.json.id);
        const collector = DIRECT_TURNS ? null : sseCollector(base, token, sessionId);
        let throughSeq = 0;
        let lastDigest = null;
        let applied = 0;
        let refused = 0;
        let previousAfter = 0;
        let observedNewest = 0;
        const turnSamples = [];
        const compactionSamples = [];
        const compactionBefore = [];
        try {
          for (let turn = 1; turn <= TURNS; turn += 1) {
            const started = performance.now();
            const text = `turn ${turn} ${'payload '.repeat(64)}`;
            if (DIRECT_TURNS) {
              const appended = spawnSync(
                appendTurn,
                ['--data-dir', data, '--session', sessionId, '--turn', String(turn), '--text', text],
                { encoding: 'utf8' },
              );
              assert(
                'turns-compaction',
                appended.status === 0,
                `append turn ${turn} failed: ${String(appended.stderr ?? '').slice(-300)}`,
              );
              const appendedRow = JSON.parse(appended.stdout.trim().split('\n').pop());
              assert('turns-compaction', appendedRow.assistantSeq > appendedRow.userSeq, `turn ${turn} seq order`);
              observedNewest = Number(appendedRow.assistantSeq);
            } else {
              const before = collector.events.length;
              const promptBody = () => ({
                session_id: sessionId,
                submission_id: randomUUID(),
                prompt: text,
              });
              let prompted = await postJson(base, token, `/native/session/${sessionId}/prompt`, promptBody());
              // A prior turn can settle its assistant row before the drive
              // and its live shadow reach a terminal state. The prompt route
              // then answers the typed 409 "interrupted run must be resumed
              // or cancelled". Settle that drive through the same cancel
              // route the runtime exposes, then retry the same prompt once
              // with a fresh submission id (the refused one never ran).
              if (prompted.status === 409 && String(prompted.text).includes('live shadow')) {
                // Prefer natural settlement (the run watch integrates or
                // discards its shadow); cancelling immediately races that
                // watcher and can leave a Discarded shadow pointer.
                let settled = await waitSessionIdle(base, token, sessionId, 20_000);
                if (!settled) {
                  settled = await settleSessionRuns(base, token, sessionId, 30_000);
                  await new Promise((done) => setTimeout(done, 250));
                }
                assert('turns-compaction', settled, `prompt ${turn} live shadow did not settle`);
                prompted = await postJson(base, token, `/native/session/${sessionId}/prompt`, promptBody());
              }
              assert('turns-compaction', prompted.status === 200, `prompt ${turn} -> ${prompted.status} ${prompted.text.slice(0, 300)}`);
              try {
                await waitForTurn(collector, before, 60_000);
              } catch (error) {
                throw new Error(`turn ${turn}: ${error.message}`);
              }
            }
            turnSamples.push(performance.now() - started);

            // Every prompt is followed by one forced, durable compaction.
            const compactionStarted = performance.now();
            const compact = await postJson(
              base,
              token,
              `/native/session/${sessionId}/context/compaction`,
              {
                operationId: `scale-${RUN_ID}-${turn}`,
                minReductionRatio: 0.1,
                maxHistoryMessages: 200,
              },
            );
            const compactionMs = performance.now() - compactionStarted;
            assert('turns-compaction', compact.status === 200, `compaction ${turn} -> ${compact.status} ${compact.text.slice(0, 300)}`);
            const op = compact.json;
            const result = op.result ?? {};
            if (op.state === 'refused') {
              assert('turns-compaction', turn <= 3, `compaction ${turn} refused unexpectedly`);
              assert('turns-compaction', result.accepted === false, `compaction ${turn} refusal claims acceptance`);
              assert('turns-compaction', typeof result.rejectedReason === 'string' && result.rejectedReason.length > 0, `compaction ${turn} refusal has no reason`);
              refused += 1;
              continue;
            }
            assert('turns-compaction', op.state === 'applied', `compaction ${turn} state ${op.state} result=${JSON.stringify(result).slice(0, 500)}`);
            applied += 1;
            // Sample ONLY applied compactions: `compactionBefore` is pushed
            // for the same rows, so the per-1k-token curve can never index an
            // undefined input (quick/early refusals used to produce NaN).
            compactionSamples.push(compactionMs);
            assert('turns-compaction', result.accepted === true, `compaction ${turn} not accepted`);
            assert('turns-compaction', result.after < result.before, `compaction ${turn} did not reduce (${result.before}->${result.after})`);
            assert(
              'turns-compaction',
              result.reduction + 1e-9 >= result.minimumReduction,
              `compaction ${turn} reduction ${result.reduction} < min ${result.minimumReduction}`,
            );
            assert('turns-compaction', /^[0-9a-f]{64}$/.test(String(result.summaryDigest)), `compaction ${turn} digest malformed`);
            assert(
              'turns-compaction',
              Number(result.checkpointThroughSeq) >= throughSeq,
              `compaction ${turn} checkpoint regressed ${result.checkpointThroughSeq} < ${throughSeq}`,
            );
            assert(
              'turns-compaction',
              Number(result.checkpointThroughSeq) >= observedNewest - 1,
              `compaction ${turn} checkpoint ${result.checkpointThroughSeq} is behind durable newest ${observedNewest}`,
            );
            throughSeq = Number(result.checkpointThroughSeq);
            assert(
              'turns-compaction',
              Number(result.keptRecentTurns) <= 200,
              `compaction ${turn} kept ${result.keptRecentTurns} turns (unbounded checkpoint)`,
            );
            assert(
              'turns-compaction',
              result.after <= previousAfter + 2_048,
              `compaction ${turn} after ${result.after} grew past the bounded window (${previousAfter})`,
            );
            previousAfter = Number(result.after);
            lastDigest = result.summaryDigest;
            compactionBefore.push(Math.max(1, Number(result.before)));
            if (!DIRECT_TURNS && turn % 100 === 0) {
              const newest = await getJson(base, token, `/native/messages?session=${sessionId}&limit=1`);
              assert('turns-compaction', newest.status === 200 && Array.isArray(newest.json.messages) && newest.json.messages.length === 1, `turn ${turn} newest read`);
              observedNewest = Number(newest.json.messages[0].seq);
            }
          }
        } finally {
          if (collector !== null) {
            collector.closed = true;
            collector.controller.abort();
            await Promise.race([collector.done, new Promise((done) => setTimeout(done, 2_000))]);
          }
        }
        assert('turns-compaction', applied + refused === TURNS, `compaction accounting ${applied}+${refused} != ${TURNS}`);
        assert('turns-compaction', applied >= TURNS - 3, `only ${applied} compactions applied`);
        assert('turns-compaction', lastDigest !== null, 'no applied compaction digest');
        const newest = await getJson(base, token, `/native/messages?session=${sessionId}&limit=1`);
        assert('turns-compaction', newest.status === 200 && newest.json.messages.length === 1, 'newest durable message read');
        const newestSeq = Number(newest.json.messages[0].seq);
        assert('turns-compaction', newestSeq >= TURNS * 2 - 1, `newest durable seq ${newestSeq} < ${TURNS * 2 - 1}`);
        assert('turns-compaction', throughSeq === newestSeq, `checkpoint ${throughSeq} != newest durable seq ${newestSeq}`);
        const latest = await getJson(base, token, `/native/session/${sessionId}/context/compaction`);
        assert('turns-compaction', latest.status === 200, 'latest compaction read');
        assert('turns-compaction', latest.json.result?.summaryDigest === lastDigest, 'latest compaction digest is stale');
        const decile = Math.max(1, Math.floor(TURNS / 10));
        const turnCurve =
          percentile(turnSamples.slice(-decile), 0.5) /
          Math.max(percentile(turnSamples.slice(0, decile), 0.5), 0.05);
        // Forced compaction's INPUT grows with the retained window (2
        // messages/turn, capped by maxHistoryMessages), so absolute last/first
        // latency is expected to rise. Normalize by the measured input tokens:
        // a super-linear cost shows up as ms-per-1k-token growth.
        const perKTok = (start, length) => {
          const values = [];
          for (let i = start; i < start + length; i += 1) {
            values.push((compactionSamples[i] * 1000) / compactionBefore[i]);
          }
          return percentile(values, 0.5);
        };
        const compDecile = Math.max(1, Math.floor(compactionSamples.length / 10));
        const compactionCurve =
          perKTok(compactionSamples.length - compDecile, compDecile) /
          Math.max(perKTok(0, compDecile), 0.0001);
        const midDecile = Math.max(1, Math.floor(TURNS / 2));
        const firstP50 = percentile(turnSamples.slice(0, decile), 0.5);
        const midP50 = percentile(turnSamples.slice(midDecile, midDecile + decile), 0.5);
        const lastP50 = percentile(turnSamples.slice(-decile), 0.5);
        const steadyCurve = lastP50 / Math.max(midP50, 0.05);
        assert('turns-compaction', turnCurve <= 20, `turn latency curve ${turnCurve.toFixed(2)} looks super-linear`);
        assert(
          'turns-compaction',
          steadyCurve <= 2,
          `steady-state turn latency curve ${steadyCurve.toFixed(2)} is not a plateau ` +
            `(first p50 ${firstP50.toFixed(3)}ms, mid p50 ${midP50.toFixed(3)}ms, last p50 ${lastP50.toFixed(3)}ms)`,
        );
        assert('turns-compaction', percentile(turnSamples, 0.95) <= 15_000, 'p95 turn latency exceeds 15s');
        assert('turns-compaction', compactionCurve <= 3, `compaction ms/1k-token curve ${compactionCurve.toFixed(2)} looks super-linear`);
        return {
          session: sessionId,
          turns: TURNS,
          mode: DIRECT_TURNS ? 'durable-append' : 'real-prompt',
          compactions: compactionSamples.length,
          applied,
          refused,
          turn_ms: summary(turnSamples),
          compaction_ms: summary(compactionSamples),
          checkpoint_through_seq: throughSeq,
          newest_durable_seq: newestSeq,
          bounded_after: previousAfter,
          newest_digest: latest.json.result?.summaryDigest ?? null,
          turn_curve: Number(turnCurve.toFixed(2)),
          turn_steady_curve: Number(steadyCurve.toFixed(2)),
          compaction_ms_per_ktok_curve: Number(compactionCurve.toFixed(2)),
        };
      });
    }

    // A one-turn history must fail typed, never panic the daemon.
    await probe('compaction-tiny-history-typed', async () => {
      const tinyWorkspace = join(tmp, 'tiny-workspace');
      mkdirSync(tinyWorkspace, { recursive: true });
      const created = await postJson(base, token, '/native/session', {
        provider: 'p1',
        model: 'm',
        workspace: tinyWorkspace,
        title: 'tiny compaction',
      });
      assert('compaction-tiny-history-typed', created.status === 200, 'create tiny session');
      const sessionId = String(created.json.id);
      const prompt = await postJson(base, token, `/native/session/${sessionId}/prompt`, {
        session_id: sessionId,
        submission_id: randomUUID(),
        prompt: 'x',
      });
      assert('compaction-tiny-history-typed', prompt.status === 200, `tiny prompt ${prompt.status}`);
      const compact = await postJson(base, token, `/native/session/${sessionId}/context/compaction`, {
        operationId: `tiny-${RUN_ID}`,
        targetTokens: 1,
        minReductionRatio: 0.2,
      });
      assert('compaction-tiny-history-typed', compact.status === 200, `tiny compaction ${compact.status}`);
      assert(
        'compaction-tiny-history-typed',
        ['failed', 'refused'].includes(compact.json.state),
        `tiny compaction state ${compact.json.state}`,
      );
      // A refused/failed compaction must carry a typed reason. The runtime
      // reports validation refusals as `result.rejectedReason` and malformed
      // plans as `error.{code,message}`; both are typed, neither is a panic.
      const rejectedReason = compact.json.result?.rejectedReason ??
        compact.json.error?.message ?? null;
      assert(
        'compaction-tiny-history-typed',
        typeof rejectedReason === 'string' && rejectedReason.length > 0,
        `tiny compaction refusal is not typed: ${compact.text.slice(0, 300)}`,
      );
      const alive = await getJson(base, token, '/native/ready');
      assert('compaction-tiny-history-typed', alive.status === 200, 'daemon died on tiny compaction');
      return { state: compact.json.state, rejected_reason: rejectedReason };
    });

    // ----------------------------------------------------------- transcript
    await probe('transcript-100k-page-search', async () => {
      const sessionId = String(ids.session);
      const pages = [];
      let before = null;
      let seen = 0;
      const sequences = new Set();
      for (;;) {
        const path = `/native/messages?session=${sessionId}&limit=200${before === null ? '' : `&before=${before}`}`;
        const started = performance.now();
        const page = await getJson(base, token, path);
        pages.push(performance.now() - started);
        assert('transcript-100k-page-search', page.status === 200, `page ${page.status}`);
        assert('transcript-100k-page-search', page.json.messages.length <= 200, `page ${page.json.messages.length} > 200`);
        for (const message of page.json.messages) {
          assert('transcript-100k-page-search', !sequences.has(message.seq), `duplicate seq ${message.seq}`);
          sequences.add(message.seq);
        }
        seen += page.json.messages.length;
        if (!page.json.hasMore) break;
        assert('transcript-100k-page-search', page.json.nextBefore !== null, 'hasMore without nextBefore');
        before = page.json.nextBefore;
      }
      assert('transcript-100k-page-search', seen === MESSAGES, `paged ${seen} != ${MESSAGES}`);
      assert('transcript-100k-page-search', sequences.size === MESSAGES, `unique ${sequences.size}`);
      const firstPage = percentile(pages.slice(0, 10), 0.5);
      const lastPage = percentile(pages.slice(-10), 0.5);
      const pageCurve = Math.max(lastPage, 0.05) / Math.max(firstPage, 0.05);
      assert('transcript-100k-page-search', pageCurve <= 6, `page latency curve ${pageCurve.toFixed(2)} looks super-linear`);
      assert('transcript-100k-page-search', percentile(pages, 0.95) <= 500, 'p95 page latency exceeds 500ms');

      async function search(session, query, limit = 200) {
        const started = performance.now();
        const found = await getJson(
          base,
          token,
          `/native/transcript/search?session=${session}&q=${encodeURIComponent(query)}&limit=${limit}`,
        );
        return { ms: performance.now() - started, found };
      }
      const userNeedle = await search(sessionId, 'user-needle-0');
      assert('transcript-100k-page-search', userNeedle.found.status === 200, `user search ${userNeedle.found.status}`);
      assert('transcript-100k-page-search', userNeedle.found.json.matches.length === 1, `user needle matches ${userNeedle.found.json.matches.length}`);
      const needleIndex = MESSAGES >= 2000 ? Math.floor(MESSAGES / 1000 - 1) * 1000 + 1 : 1;
      const assistantNeedle = await search(sessionId, `assistant-needle-${needleIndex} ünïcode-prefix`);
      assert(
        'transcript-100k-page-search',
        assistantNeedle.found.status === 200 && assistantNeedle.found.json.matches.length === 1,
        `assistant part search misses the answer (${assistantNeedle.found.status}, ${assistantNeedle.found.json?.matches?.length})`,
      );
      assert(
        'transcript-100k-page-search',
        assistantNeedle.found.json.matches[0].snippet.includes(`assistant-needle-${needleIndex}`),
        'assistant snippet is empty or wrong',
      );
      const unicode = await search(sessionId, 'ünïcode-prefix');
      assert('transcript-100k-page-search', unicode.found.status === 200, `unicode search ${unicode.found.status}`);
      assert('transcript-100k-page-search', unicode.found.json.matches.length > 0, 'unicode part search returned nothing');
      assert(
        'transcript-100k-page-search',
        unicode.found.json.matches.every((match) => match.role === 'assistant'),
        'unicode search surfaced a non-assistant row',
      );
      const seqs = unicode.found.json.matches.map((match) => match.seq);
      assert(
        'transcript-100k-page-search',
        seqs.every((seq, index) => index === 0 || seqs[index - 1] > seq),
        'search is not newest-first',
      );
      const smallNeedle = await search(String(ids.small_session), 'small-needle-last');
      assert(
        'transcript-100k-page-search',
        smallNeedle.found.status === 200 && smallNeedle.found.json.matches.length === 1,
        `small transcript search ${smallNeedle.found.status}`,
      );
      const largeSamples = [];
      const smallSamples = [];
      for (let index = 0; index < 20; index += 1) {
        // Same selectivity on both transcripts (zero matches => both scan
        // their WHOLE durable history, including part rows). Comparing a
        // dense query on the 10k transcript with a sparse one on the 100k
        // transcript measured match density, not scaling.
        const absent = `absent-${RUN_ID}`;
        largeSamples.push((await search(sessionId, absent)).ms);
        smallSamples.push((await search(String(ids.small_session), absent)).ms);
      }
      const largeMs = percentile(largeSamples, 0.5);
      const smallMs = percentile(smallSamples, 0.5);
      const ratio = Math.max(largeMs, 0.05) / Math.max(smallMs, 0.05);
      assert('transcript-100k-page-search', ratio <= 25, `search ratio ${ratio.toFixed(1)} looks super-linear (10k=${smallMs.toFixed(3)}ms, 100k=${largeMs.toFixed(3)}ms)`);
      return {
        messages: seen,
        pages: pages.length,
        page_ms: summary(pages),
        search_100k_ms: summary(largeSamples),
        search_10k_ms: summary(smallSamples),
        search_ratio_10x: Number(ratio.toFixed(2)),
        page_curve_first_last: Number(pageCurve.toFixed(2)),
        assistant_part_search: true,
        unicode_matches: unicode.found.json.matches.length,
      };
    });

    // --------------------------------------------------------------- memory
    await probe('memory-5000', async () => {
      const memoryWorkspace = join(tmp, 'memory-workspace');
      mkdirSync(memoryWorkspace, { recursive: true });
      const created = await postJson(base, token, '/native/session', {
        provider: 'p1',
        model: 'm',
        workspace: memoryWorkspace,
        title: 'memory scale',
      });
      assert('memory-5000', created.status === 200, `create memory session ${created.status}`);
      const sessionId = String(created.json.id);
      const createSamples = [];
      const idsList = [];
      const createdIds = new Set();
      for (let index = 0; index < MEMORY_FACTS; index += 1) {
        const started = performance.now();
        const subject = `svc-${String(index).padStart(5, '0')}`;
        const body = await postJson(base, token, '/native/memory/records', {
          session: sessionId,
          scope: 'session',
          subject,
          predicate: 'port',
          value: { type: 'number', value: index },
          provenance: { origin: 'user' },
        });
        createSamples.push(performance.now() - started);
        assert('memory-5000', body.status === 200, `memory create ${index} -> ${body.status} ${body.text.slice(0, 200)}`);
        const record = body.json.record;
        assert('memory-5000', record.subject === subject, `memory create ${index} returned subject ${record.subject}`);
        assert('memory-5000', !createdIds.has(record.id), `memory create ${index} reused id ${record.id}`);
        createdIds.add(record.id);
        idsList.push(record.id);
      }
      if (MEMORY_FACTS >= 5000) {
        const over = await postJson(base, token, '/native/memory/records', {
          session: sessionId,
          scope: 'session',
          subject: 'svc-overflow',
          predicate: 'port',
          value: { type: 'number', value: 1 },
        });
        assert(
          'memory-5000',
          over.status === 409 && over.text.includes('memory_policy_limit'),
          `policy overflow not typed: ${over.status} ${over.text.slice(0, 200)}`,
        );
      }
      const first = createSamples.slice(0, Math.max(1, Math.floor(MEMORY_FACTS / 10)));
      const last = createSamples.slice(-Math.max(1, Math.floor(MEMORY_FACTS / 10)));
      const curve = percentile(last, 0.5) / Math.max(percentile(first, 0.5), 0.05);
      assert('memory-5000', curve <= 8, `memory create cost ratio ${curve.toFixed(2)} across ${MEMORY_FACTS} rows looks quadratic`);

      const editSamples = [];
      for (let index = 0; index < Math.min(1000, MEMORY_FACTS); index += 1) {
        const started = performance.now();
        const edited = await fetch(
          `${base}/native/memory/records/${encodeURIComponent(idsList[index])}`,
          {
            method: 'PUT',
            headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' },
            body: JSON.stringify({
              session: sessionId,
              expectedVersion: 1,
              value: { type: 'number', value: index + 1 },
            }),
          },
        ).then(async (response) => ({
          status: response.status,
          json: await response.json().catch(() => null),
          text: '',
        }));
        editSamples.push(performance.now() - started);
        assert('memory-5000', edited.status === 200 && edited.json.record.version === 2, `memory edit ${index} -> ${edited.status}`);
      }
      const deleteSamples = [];
      const deletedIds = new Set();
      for (let index = 0; index < Math.min(1000, MEMORY_FACTS); index += 1) {
        const started = performance.now();
        const deleted = await fetch(
          `${base}/native/memory/records/${encodeURIComponent(idsList[index])}?session=${sessionId}&expectedVersion=2`,
          { method: 'DELETE', headers: { authorization: `Bearer ${token}` } },
        );
        const text = await deleted.text();
        deleteSamples.push(performance.now() - started);
        assert('memory-5000', deleted.status === 200, `memory delete ${index} -> ${deleted.status} ${text.slice(0, 200)}`);
        deletedIds.add(idsList[index]);
      }
      assert('memory-5000', deletedIds.size === Math.min(1000, MEMORY_FACTS), `deleted ${deletedIds.size} distinct rows`);

      let before = null;
      let live = 0;
      const seen = new Set();
      for (;;) {
        const query = new URLSearchParams({ session: sessionId, limit: '200', scope: 'session' });
        if (before !== null) {
          query.set('afterMs', String(before.afterMs));
          query.set('afterKind', String(before.afterKind));
          query.set('afterKey', String(before.afterKey));
        }
        const page = await getJson(base, token, `/native/memory/records?${query}`);
        assert('memory-5000', page.status === 200, `memory page ${page.status}`);
        for (const record of page.json.records) {
          assert('memory-5000', !seen.has(record.id), `duplicate memory ${record.id}`);
          seen.add(record.id);
          assert('memory-5000', record.scope === 'session' && Number(record.scopeId) === Number(sessionId), 'memory scope escaped its session');
          assert('memory-5000', record.deleted === null, 'default memory page served a tombstone');
        }
        live += page.json.records.length;
        if (!page.json.hasMore) break;
        before = page.json.nextCursor;
        assert('memory-5000', before !== null, 'memory hasMore without cursor');
      }
      const expectedLive = [...createdIds].filter((id) => !deletedIds.has(id));
      if (live !== expectedLive.length) {
        const missing = expectedLive.filter((id) => !seen.has(id));
        console.error(
          `[scale] memory-5000: live=${live} expected=${expectedLive.length} created=${createdIds.size} deleted=${deletedIds.size} missing=${missing.length} ${missing.slice(0, 5).join(',')}`,
        );
      }
      assert('memory-5000', live === expectedLive.length, `live memory ${live} != expected ${expectedLive.length}`);

      const all = await getJson(base, token, `/native/memory/records?session=${sessionId}&limit=200&scope=session&includeDeleted=true`);
      assert('memory-5000', all.status === 200 && all.json.records.length === 200, 'tombstone page size');
      const retention = await postJson(base, token, '/native/memory/records/retention', {
        session: sessionId,
        scope: 'session',
        olderThanDays: 0,
        dryRun: false,
        limit: 1000,
      });
      assert('memory-5000', retention.status === 200, `retention ${retention.status} ${retention.text.slice(0, 200)}`);
      assert('memory-5000', retention.json.deleted === Math.min(1000, MEMORY_FACTS), `retention deleted ${retention.json.deleted}`);
      assert('memory-5000', retention.json.truncated === false || retention.json.scanned <= 4096, 'retention scan unbounded');
      assert('memory-5000', Number(retention.json.scanned) <= 4096, `retention scanned ${retention.json.scanned} rows`);

      const workspaceFact = await postJson(base, token, '/native/memory/records', {
        session: sessionId,
        scope: 'workspace',
        subject: 'workspace-scope',
        predicate: 'only',
        value: { type: 'bool', value: true },
      });
      assert('memory-5000', workspaceFact.status === 200, `workspace memory ${workspaceFact.status}`);
      const sessionOnly = await getJson(base, token, `/native/memory/records?session=${sessionId}&scope=session&limit=200`);
      assert(
        'memory-5000',
        sessionOnly.json.records.every((record) => record.scope === 'session'),
        'session scope leaked a workspace fact',
      );
      const merged = await getJson(base, token, `/native/memory/records?session=${sessionId}&scope=all&limit=200`);
      assert('memory-5000', merged.status === 200 && merged.json.records.length > 0, 'scope=all read failed');
      return {
        facts: MEMORY_FACTS,
        edits: editSamples.length,
        deletes: deleteSamples.length,
        live,
        retention_deleted: retention.json.deleted,
        create_ms: summary(createSamples),
        edit_ms: summary(editSamples),
        delete_ms: summary(deleteSamples),
        create_curve_first_last: Number(curve.toFixed(2)),
      };
    });

    // -------------------------------------------------------------- history
    await probe('history-2000', async () => {
      let before = null;
      let snapshotRevision = null;
      let snapshotMs = null;
      let total = 0;
      const seen = new Set();
      const pageSamples = [];
      let lastRevision = Number.MAX_SAFE_INTEGER;
      let lastId = Number.MAX_SAFE_INTEGER;
      for (;;) {
        const query = new URLSearchParams({ limit: '200' });
        if (before !== null) {
          // The durable cursor is the monotonic `(listing_revision, id)`
          // pair; wall-clock `beforeMs`/`snapshotMs` are presentation
          // metadata and never bound the page.
          assert('history-2000', snapshotRevision !== null, 'cursored page without a snapshot');
          assert('history-2000', before.beforeRevision !== undefined, 'nextBefore lacks beforeRevision');
          query.set('beforeRevision', String(before.beforeRevision));
          query.set('beforeId', String(before.beforeId));
          query.set('snapshotRevision', String(snapshotRevision));
          if (before.beforeMs !== undefined) query.set('beforeMs', String(before.beforeMs));
          if (snapshotMs !== null) query.set('snapshotMs', String(snapshotMs));
        }
        const started = performance.now();
        const page = await getJson(base, token, `/native/sessions?${query}`);
        pageSamples.push(performance.now() - started);
        assert('history-2000', page.status === 200, `history page ${page.status}`);
        assert('history-2000', page.json.sessions.length <= 200, `history page ${page.json.sessions.length} > 200`);
        if (snapshotRevision === null) {
          snapshotRevision = Number(page.json.snapshotRevision);
          snapshotMs = Number(page.json.snapshotMs);
          assert('history-2000', Number.isFinite(snapshotRevision) && snapshotRevision >= 0, 'first page carries no snapshotRevision');
          assert('history-2000', Number.isFinite(snapshotMs) && snapshotMs >= 0, 'first page carries no snapshotMs');
        } else {
          assert('history-2000', Number(page.json.snapshotRevision) === snapshotRevision, 'snapshot revision changed mid-walk');
          assert('history-2000', Number(page.json.snapshotMs) === snapshotMs, 'snapshot ms changed mid-walk');
        }
        for (const row of page.json.sessions) {
          assert('history-2000', !seen.has(row.id), `duplicate history session ${row.id}`);
          seen.add(row.id);
          const revision = Number(row.listing_revision);
          const id = Number(row.id);
          assert(
            'history-2000',
            revision < lastRevision || (revision === lastRevision && id < lastId),
            'history listing is not newest-first/stable',
          );
          lastRevision = revision;
          lastId = id;
        }
        total += page.json.sessions.length;
        if (!page.json.hasMore) break;
        before = page.json.nextBefore;
        assert('history-2000', before !== null, 'history hasMore without cursor');
      }
      assert('history-2000', total >= SESSIONS, `history total ${total} < ${SESSIONS}`);
      assert(
        'history-2000',
        seen.has(String(ids.history_first)) && seen.has(String(ids.history_last)),
        'history paging lost the oldest or newest seeded conversation',
      );
      const firstPage = percentile(pageSamples.slice(0, 3), 0.5);
      const lastPage = percentile(pageSamples.slice(-3), 0.5);
      const pageCurve = Math.max(lastPage, 0.05) / Math.max(firstPage, 0.05);
      assert('history-2000', pageCurve <= 6, `history page latency curve ${pageCurve.toFixed(2)} looks super-linear`);
      return {
        sessions: total,
        seeded: SESSIONS,
        pages: pageSamples.length,
        page_ms: summary(pageSamples),
        page_curve_first_last: Number(pageCurve.toFixed(2)),
        paging: 'exact, no duplicates, stable newest-first',
      };
    });

    // --------------------------------------------------------------- agents
    await probe('child-agents-500', async () => {
      const started = performance.now();
      const agents = await getJson(base, token, `/native/agents?session=${ids.session}`);
      const ms = performance.now() - started;
      assert('child-agents-500', agents.status === 200, `agents ${agents.status} ${agents.text.slice(0, 200)}`);
      assert('child-agents-500', agents.json.length <= 500, `agents ${agents.json.length} exceeds the 500 projection cap`);
      const children = agents.json.filter((entry) => entry.kind === 'child');
      const unique = new Set(agents.json.map((entry) => entry.agent_id));
      assert('child-agents-500', unique.size === agents.json.length, 'duplicate agent entries');
      assert('child-agents-500', children.length >= Math.min(CHILDREN, 495), `only ${children.length} children projected`);
      const taskRuns = await getJson(base, token, `/native/session/${ids.session}/task-runs`);
      assert('child-agents-500', taskRuns.status === 200 && taskRuns.json.length <= 500, 'task-runs not bounded');
      return {
        seeded_children: CHILDREN,
        projected_entries: agents.json.length,
        projected_children: children.length,
        agents_ms: Number(ms.toFixed(3)),
        bounded: true,
      };
    });

    // --------------------------------------------------------------- claims
    await probe('proof-10000-claims', async () => {
      const started = performance.now();
      const proof = await getJson(
        base,
        token,
        `/native/tasks/${ids.claim_task}/proof?session=${ids.session}`,
      );
      const ms = performance.now() - started;
      assert('proof-10000-claims', proof.status === 200, `proof ${proof.status} ${proof.text.slice(0, 300)}`);
      assert('proof-10000-claims', proof.json.criteria.total === CLAIMS, `criteria total ${proof.json.criteria.total}`);
      assert('proof-10000-claims', proof.json.checks.total === CLAIMS, `checks total ${proof.json.checks.total}`);
      assert('proof-10000-claims', proof.json.criteria.passed === CLAIMS / 2, `criteria passed ${proof.json.criteria.passed}`);
      assert('proof-10000-claims', proof.json.criteria.failed === CLAIMS / 2, `criteria failed ${proof.json.criteria.failed}`);
      assert('proof-10000-claims', proof.json.criteria.unavailable === 0, `criteria unavailable ${proof.json.criteria.unavailable}`);
      assert('proof-10000-claims', proof.json.checks.passed === CLAIMS / 2, `checks passed ${proof.json.checks.passed}`);
      assert('proof-10000-claims', proof.json.checks.failed === CLAIMS / 2, `checks failed ${proof.json.checks.failed}`);
      assert('proof-10000-claims', proof.json.checks.requiredTotal === CLAIMS / 2, `required total ${proof.json.checks.requiredTotal}`);
      assert('proof-10000-claims', proof.json.checks.requiredPassed === CLAIMS / 2, `required passed ${proof.json.checks.requiredPassed}`);
      assert('proof-10000-claims', proof.json.checks.requiredAllPassed === true, 'required checks did not all pass');
      assert('proof-10000-claims', proof.json.criteria.items.length <= 500, 'criteria page exceeded 500');
      assert('proof-10000-claims', proof.json.checks.items.length <= 500, 'checks page exceeded 500');
      assert('proof-10000-claims', proof.json.criteria.truncated === true, 'criteria truncation not declared');
      assert('proof-10000-claims', proof.json.checks.truncated === true, 'checks truncation not declared');
      assert('proof-10000-claims', proof.json.criteria.passed + proof.json.criteria.failed + proof.json.criteria.unavailable <= CLAIMS, 'criteria counts overflow');
      assert('proof-10000-claims', proof.text.length < 4 * 1024 * 1024, `proof payload ${proof.text.length} bytes is unbounded`);
      return {
        claims: CLAIMS,
        served_criteria: proof.json.criteria.items.length,
        served_checks: proof.json.checks.items.length,
        criteria_passed: proof.json.criteria.passed,
        criteria_failed: proof.json.criteria.failed,
        checks_passed: proof.json.checks.passed,
        checks_failed: proof.json.checks.failed,
        truncated: true,
        proof_ms: Number(ms.toFixed(3)),
        bytes: proof.text.length,
      };
    });

    // ------------------------------------------------------ MCP/skills/hooks
    await probe('mcp-skills-hooks-500', async () => {
      const mcpSamples = [];
      for (let index = 0; index < 8; index += 1) {
        const started = performance.now();
        const saved = await postJson(base, token, '/native/mcp/servers', {
          name: `server-${String(index).padStart(4, '0')}`,
          command: '/bin/true',
          trusted: true,
          enabled: false,
        });
        mcpSamples.push(performance.now() - started);
        assert('mcp-skills-hooks-500', saved.status === 200, `mcp save ${index} -> ${saved.status} ${saved.text.slice(0, 160)}`);
      }
      const mcpOver = await postJson(base, token, '/native/mcp/servers', {
        name: 'server-overflow',
        command: '/bin/true',
        trusted: true,
        enabled: false,
      });
      assert(
        'mcp-skills-hooks-500',
        mcpOver.status === 400 && mcpOver.text.includes('at most 8 MCP servers'),
        `MCP cap not typed: ${mcpOver.status} ${mcpOver.text.slice(0, 160)}`,
      );
      const mcpList = await getJson(base, token, '/native/mcp/servers?limit=500');
      assert('mcp-skills-hooks-500', mcpList.status === 200 && mcpList.json.servers.length === 8, `mcp list ${mcpList.json?.servers?.length}`);

      const hookSamples = [];
      for (let index = 0; index < 256; index += 1) {
        const started = performance.now();
        const saved = await postJson(base, token, '/native/hooks', {
          id: `hook-${String(index).padStart(4, '0')}`,
          spec: {
            id: `hook-${String(index).padStart(4, '0')}`,
            events: ['task_complete'],
            command: '/bin/true',
            args: [],
            env: [],
            env_allowlist: true,
            deadline_ms: 1000,
            stdout_cap: 1024,
            stderr_cap: 1024,
            failure_policy: 'warn',
            permission_scope: [],
          },
          trusted: true,
          enabled: false,
        });
        hookSamples.push(performance.now() - started);
        assert('mcp-skills-hooks-500', saved.status === 200, `hook save ${index} -> ${saved.status} ${saved.text.slice(0, 160)}`);
      }
      const hookOver = await postJson(base, token, '/native/hooks', {
        id: 'hook-overflow',
        spec: {
          id: 'hook-overflow',
          events: ['task_complete'],
          command: '/bin/true',
          args: [],
          env: [],
          env_allowlist: true,
          deadline_ms: 1000,
          stdout_cap: 1024,
          stderr_cap: 1024,
          failure_policy: 'warn',
          permission_scope: [],
        },
        trusted: true,
        enabled: false,
      });
      assert(
        'mcp-skills-hooks-500',
        hookOver.status === 400 && hookOver.text.includes('at most 256 hooks'),
        `hook cap not typed: ${hookOver.status} ${hookOver.text.slice(0, 160)}`,
      );
      const hookList = await getJson(base, token, '/native/hooks?limit=500');
      assert('mcp-skills-hooks-500', hookList.status === 200 && hookList.json.hooks.length === 256, `hook list ${hookList.json?.hooks?.length}`);

      const skillRoot = join(tmp, 'skill-workspace');
      for (let index = 0; index < 500; index += 1) {
        const dir = join(skillRoot, '.faktor', 'skills', `skill-${String(index).padStart(4, '0')}`);
        mkdirSync(dir, { recursive: true });
        writeFileSync(
          join(dir, 'SKILL.md'),
          `# skill ${index}\n\nUse this skill for scale item ${index}.\n`,
        );
      }
      const created = await postJson(base, token, '/native/session', {
        provider: 'p1',
        model: 'm',
        workspace: skillRoot,
        title: 'skill scale',
      });
      assert('mcp-skills-hooks-500', created.status === 200, `skill session ${created.status}`);
      const skillSession = String(created.json.id);
      const skills = await getJson(base, token, `/native/skills?session=${skillSession}`);
      assert('mcp-skills-hooks-500', skills.status === 200, `skills ${skills.status} ${skills.text.slice(0, 200)}`);
      const skillRows = skills.json.skills ?? skills.json.entries ?? [];
      assert('mcp-skills-hooks-500', skillRows.length === 500, `skills ${skillRows.length}`);
      const names = new Set(skillRows.map((row) => row.name));
      assert('mcp-skills-hooks-500', names.size === 500, 'duplicate skill names');
      return {
        mcp_servers: 8,
        hooks: 256,
        skills: 500,
        mcp_cap: 8,
        hook_cap: 256,
        mcp_save_ms: summary(mcpSamples),
        hook_save_ms: summary(hookSamples),
      };
    });

    // ---------------------------------------------------- concurrent streams
    await probe('concurrent-16-stream', async () => {
      const sessionIds = [];
      for (let index = 0; index < STREAM_SESSIONS; index += 1) {
        const streamWorkspace = join(tmp, `stream-workspace-${index}`);
        mkdirSync(streamWorkspace, { recursive: true });
        const created = await postJson(base, token, '/native/session', {
          provider: 'p1',
          model: 'm',
          workspace: streamWorkspace,
          title: `stream ${index}`,
        });
        assert('concurrent-16-stream', created.status === 200, `stream session ${index} ${created.status}`);
        sessionIds.push(String(created.json.id));
      }
      const collectors = sessionIds.map((id) => sseCollector(base, token, id));
      await new Promise((done) => setTimeout(done, 250));
      const promptSamples = [];
      await Promise.all(
        sessionIds.map(async (id, index) => {
          const started = performance.now();
          const prompted = await postJson(base, token, `/native/session/${id}/prompt`, {
            session_id: id,
            submission_id: randomUUID(),
            prompt: `concurrent stream ${index} ${'payload '.repeat(50)}`,
          });
          promptSamples.push(performance.now() - started);
          assert('concurrent-16-stream', prompted.status === 200, `stream prompt ${index} ${prompted.status}`);
        }),
      );
      const deadline = Date.now() + 20_000;
      while (Date.now() < deadline) {
        if (collectors.every((collector) => collector.events.some((event) => event.event === 'turn_completed'))) break;
        await new Promise((done) => setTimeout(done, 50));
      }
      for (const [index, collector] of collectors.entries()) {
        collector.closed = true;
        collector.controller.abort();
        await Promise.race([collector.done, new Promise((done) => setTimeout(done, 2000))]);
        assert('concurrent-16-stream', collector.error === null, `stream ${index} failed: ${collector.error}`);
        const idsSeen = collector.ids;
        assert('concurrent-16-stream', idsSeen.length > 0, `stream ${index} received no events`);
        assert(
          'concurrent-16-stream',
          idsSeen.every((seq, at) => at === 0 || seq > idsSeen[at - 1]),
          `stream ${index} is out of order`,
        );
        assert(
          'concurrent-16-stream',
          collector.events.some((event) => event.event === 'turn_completed'),
          `stream ${index} never observed its turn completion (${collector.events.map((event) => `${event.event}:${event.data}`).join(' | ').slice(0, 800) || 'no events'})`,
        );
        assert('concurrent-16-stream', idsSeen.length <= 256, `stream ${index} unbounded (${idsSeen.length} events)`);
      }
      const alive = await getJson(base, token, '/native/ready');
      assert('concurrent-16-stream', alive.status === 200, 'daemon unhealthy after concurrent streams');
      return {
        sessions: STREAM_SESSIONS,
        prompt_ms: summary(promptSamples),
        events_per_stream: collectors.map((collector) => collector.ids.length),
        ordered: true,
        bounded: true,
      };
    });

    // ----------------------------------------------------------- rss bound
    await probe('rss-bounded', async () => {
      const values = samples.filter((value) => value !== null);
      assert('rss-bounded', values.length > 10, `only ${values.length} RSS samples`);
      const start = startedRss;
      const peak = Math.max(...values);
      const end = rssKb(daemon.child.pid);
      assert('rss-bounded', start !== null && start > 0, 'start RSS missing');
      assert('rss-bounded', end !== null && end > 0, 'end RSS missing');
      // The daemon holds 100k messages, 5k facts, 10k claims and 16 live
      // streams; a bounded projection/queue must not retain any of that
      // growth. 512 MiB headroom is a fail-closed bound, not a target.
      assert('rss-bounded', peak - start <= 512 * 1024, `peak RSS grew ${(peak - start) / 1024} MiB`);
      assert('rss-bounded', end - start <= 512 * 1024, `end RSS grew ${(end - start) / 1024} MiB`);
      assert('rss-bounded', end <= Math.ceil(peak * 1.25), `end RSS ${end} is not near the plateau ${peak}`);
      return { start_kb: start, peak_kb: peak, end_kb: end, growth_kb: end - start };
    });

    // --------------------------------------------------------------- summary
    const peakRss = samples.filter((value) => value !== null);
    const document = {
      schema: 'faktor-edition8-scale/v1',
      generated_at: new Date().toISOString(),
      run_id: RUN_ID,
      platform: process.platform,
      quick: QUICK,
      gate_evidence: FULL_COVERAGE,
      daemon: { binary: store, pid: daemon.child.pid, log: log.replace(tmp, '<tmp>') },
      seed: ids,
      counts: {
        turns: SKIP_TURNS ? 0 : TURNS,
        compactions: SKIP_TURNS ? 0 : TURNS,
        messages: MESSAGES,
        memory_facts: MEMORY_FACTS,
        history_sessions: SESSIONS,
        children: CHILDREN,
        claims: CLAIMS,
        mcp_servers: 8,
        skills: 500,
        hooks: 256,
        concurrent_sessions: STREAM_SESSIONS,
        failed: failures.length,
        probes: probes.length,
      },
      metrics: {
        turns: {
          turn_ms: probes.find((entry) => entry.id === 'turns-compaction')?.turn_ms ?? null,
          compaction_ms:
            probes.find((entry) => entry.id === 'turns-compaction')?.compaction_ms ?? null,
        },
        rss_kb: {
          start: startedRss,
          peak: peakRss.length > 0 ? Math.max(...peakRss) : null,
          end: rssKb(daemon.child.pid),
          samples: peakRss.length,
        },
      },
      probes,
      failures,
    };
    mkdirSync(OUT, { recursive: true });
    const path = resolve(OUT, OUT_NAME);
    const payload = `${JSON.stringify(document, null, 2)}\n`;
    writeFileSync(path, payload);
    console.log(
      `[scale] wrote ${path} (${sha(Buffer.from(payload))})${FULL_COVERAGE ? '' : ' [reduced run: not gate evidence]'}`,
    );
    if (failures.length > 0) process.exit(1);
  } finally {
    if (sampler !== null) clearInterval(sampler);
    await stop(daemon?.child ?? null);
    fake.kill('SIGKILL');
    if (process.env.KEEP_TMP === '1') console.error(`[scale] kept temp dir ${tmp}`);
    else rmSync(tmp, { recursive: true, force: true });
  }
}

main().catch((error) => {
  console.error(`scale-probe: ${error && error.stack ? error.stack : String(error)}`);
  process.exit(1);
});
