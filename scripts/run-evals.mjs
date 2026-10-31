#!/usr/bin/env node
// TEST-SEPARATION: oracle=external-fixture isolation=separate-process,isolated-data-dir
// Real-provider conversation-quality eval runner (HAR-09/HAR-17).
//
// Drives a REAL throwaway daemon (temp data-dir; never ~/.faktor-live for
// state) through the conversation-quality suite:
//   greeting; conceptual explanation; code explanation (+ attachment);
//   pronoun follow-up; instruction reversal; repository lookup; compare;
//   return to earlier topic; mode escalation ("now implement it"); then a
//   short task sequence.
//
// A recording proxy sits between the daemon and the provider
// (api.deepseek.com): every provider request is captured for the gates and
// for HUMAN REVIEW, then forwarded unchanged. The suite hard-caps provider
// completions (`--max-requests`, default 20): the proxy rejects further
// calls past the cap, so a runaway task loop can never blow the budget.
//
// Gates (assertions; non-zero exit ONLY for these):
//   - turn-completed              every scripted turn finishes (or is a
//                                 deliberate cost-cap skip -> review)
//   - no-engineering-boilerplate  ask-turn provider requests carry NO task
//                                 protocol/state markers (## Working
//                                 protocol, LOCATE before editing, ## Task
//                                 progress, changed files:, ...)
//   - cross-turn-reference        turn N's recorded request contains turn
//                                 N-2's reply tokens (history captured)
//   - engineering-state-only-at-escalation  tasks/verification empty before
//                                 the escalation turn
//   - escalation-established      at the escalation turn: task ledger
//                                 non-empty AND task-profile request
//   - request-material-recorded   each completed turn recorded >=1 provider
//                                 request (the material human review needs)
//   - cost-cap                    total recorded requests <= cap
// Review items (marked `review`, NEVER fail the run): pronoun correctness,
// instruction-reversal compliance, history/context/attachment inspection,
// cost-cap skips, deliberate aborts.
//
// Modes:
//   node scripts/run-evals.mjs                     conversation suite (default)
//   node scripts/run-evals.mjs conversation [--bin PATH] [--max-requests N]
//   node scripts/run-evals.mjs fixtures            run the #[ignore]d 1M-line
//                                                  scale probe (explicit script
//                                                  for the manual lane)
//   node scripts/run-evals.mjs --selftest          gate evaluator planted-
//                                                  violation fixtures
//
// Provider env: ~/.faktor-live/deepseek.env (KEY=VALUE) when present —
// READ ONLY; SKIP-with-note (exit 0) when absent.
// Report: target/certification/evals/<timestamp>.report.json

import { spawn } from 'node:child_process';
import { randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..');
const REPORT_DIR = join(ROOT, 'target/certification/evals');
const GATE_FIXTURE_DIR = join(ROOT, 'tests/eval/fixtures/gates-selftest');
const POLYGLOT_DIR = join(ROOT, 'target/eval-fixtures/polyglot');
const HOME = process.env.HOME ?? '';
const ENV_FILE = process.env.FAKTOR_EVAL_ENV ?? join(HOME, '.faktor-live', 'deepseek.env');
const UPSTREAM_HOST = process.env.FAKTOR_EVAL_UPSTREAM ?? 'api.deepseek.com';

// Markers that distinguish the TASK engineering profile/state from the ask
// conversational profile. Evidence: crates/cli/src/config/mcp.rs (default
// task instructions: "## Working protocol", "LOCATE before editing", "4.
// VERIFY with", final-answer changed-files line), crates/agent/src/
// runtime/turn/state.rs (child-handoff "changed files: " block), crates/
// context/src/wire_plan.rs ("## Task progress"). The ask profile contains
// NEITHER "## Working protocol" NOR "LOCATE before editing" NOR "changed
// files: " (it says "changed-file summaries", hyphenated, as a prohibition).
const TASK_PROFILE_MARKERS = [
  '## Working protocol',
  'LOCATE before editing',
  '## Task progress',
  'changed files: ',
  'verification command/result',
  '4. VERIFY with',
];

const STOPWORDS = new Set([
  'the', 'and', 'that', 'this', 'with', 'for', 'from', 'have', 'has', 'had', 'were', 'been',
  'would', 'could', 'should', 'about', 'into', 'than', 'then', 'them', 'they', 'their', 'there',
  'what', 'when', 'which', 'while', 'will', 'your', 'yours', 'does', 'didn', 'also', 'like',
  'just', 'because', 'between', 'through', 'these', 'those', 'such', 'each', 'other', 'more',
  'most', 'some', 'any', 'all', 'one', 'two', 'can', 'may', 'might', 'must', 'over', 'under',
]);

// ------------------------------------------------------------------ helpers

function argValue(name, fallback = null) {
  const idx = process.argv.indexOf(`--${name}`);
  return idx >= 0 && idx + 1 < process.argv.length ? process.argv[idx + 1] : fallback;
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

function fail(message) {
  console.error(`EVAL FAIL: ${message}`);
  process.exit(1);
}

function skip(message) {
  console.log(`EVAL SKIP: ${message}`);
  process.exit(0);
}

function readProviderEnv() {
  if (!existsSync(ENV_FILE)) return null;
  const env = {};
  for (const line of readFileSync(ENV_FILE, 'utf8').split('\n')) {
    const m = /^([A-Z0-9_]+)=(.*)$/.exec(line.trim());
    if (m) env[m[1]] = m[2];
  }
  if (Object.keys(env).length === 0) return null;
  return env;
}

function resolveBinary() {
  const explicit = argValue('bin') ?? process.env.FAKTOR_EVAL_BIN;
  const candidates = [
    explicit,
    join(ROOT, 'target/debug/faktor-cli'),
    join(ROOT, 'target/debug/faktor'),
    join(HOME, '.faktor-live/bin/faktor'),
  ].filter(Boolean);
  for (const candidate of candidates) {
    if (existsSync(candidate)) return candidate;
  }
  return null;
}

function httpJson({ port, method, path, token, body }) {
  return new Promise((res, rej) => {
    const payload = body === undefined ? null : Buffer.from(JSON.stringify(body));
    const req = http.request(
      {
        host: '127.0.0.1',
        port,
        method,
        path,
        headers: {
          authorization: `Bearer ${token}`,
          ...(payload === null ? {} : { 'content-type': 'application/json' }),
        },
      },
      (r) => {
        const chunks = [];
        r.on('data', (c) => chunks.push(c));
        r.on('end', () => {
          const text = Buffer.concat(chunks).toString('utf8');
          let json = null;
          try {
            json = JSON.parse(text);
          } catch {
            /* non-json */
          }
          res({ status: r.statusCode, json, text });
        });
      },
    );
    req.on('error', rej);
    if (payload !== null) req.write(payload);
    req.end();
  });
}

// -------------------------------------------------------------- gate engine
//
// Pure function over a normalized recording — the SAME shape the live runner
// produces and the selftest fixtures plant. No SUT calls live here.

function requestTexts(turn) {
  return (turn.requests ?? []).map((r) => (typeof r === 'string' ? r : JSON.stringify(r)));
}

function distinctiveTokens(text, userPrompts) {
  const tokens = new Set();
  for (const raw of String(text ?? '').toLowerCase().split(/[^a-z0-9]+/)) {
    if (raw.length < 6 || STOPWORDS.has(raw)) continue;
    if (userPrompts.has(raw)) continue;
    tokens.add(raw);
  }
  return [...tokens];
}

function verifyShapeEmpty(v) {
  if (v === undefined || v === null) return true;
  const owed = Array.isArray(v.owed) ? v.owed.length : 0;
  const failed = Array.isArray(v.failedChecks) ? v.failedChecks.length : 0;
  return owed === 0 && failed === 0;
}

function evaluateGates(recorded) {
  const gates = [];
  const reviewItems = [];
  const userPrompts = new Set();
  for (const turn of recorded.turns) {
    for (const raw of String(turn.user_prompt ?? '').toLowerCase().split(/[^a-z0-9]+/)) {
      if (raw.length >= 6) userPrompts.add(raw);
    }
  }
  const push = (id, status, detail) => gates.push({ id, status, detail });
  const esc = recorded.escalation_index;

  for (let i = 0; i < recorded.turns.length; i += 1) {
    const turn = recorded.turns[i];
    const texts = requestTexts(turn);

    // turn-completed
    if (turn.status === 'completed') {
      push(`turn-completed@${i}`, 'pass', `${turn.id} completed`);
    } else if (turn.status === 'skipped-cost-cap') {
      push(`turn-completed@${i}`, 'review', `${turn.id} skipped at the completion cost cap`);
    } else if (turn.status === 'aborted-cost-bound') {
      push(`turn-completed@${i}`, 'review', `${turn.id} deliberately aborted to hold the cost cap`);
    } else {
      push(`turn-completed@${i}`, 'fail', `${turn.id} ended with status '${turn.status}'`);
    }

    // request-material-recorded
    if (turn.status === 'completed' || turn.status === 'aborted-cost-bound') {
      if (texts.length >= 1) {
        push(`request-material-recorded@${i}`, 'pass', `${texts.length} provider request(s) recorded`);
      } else {
        push(`request-material-recorded@${i}`, 'fail', `${turn.id} completed without a recorded provider request`);
      }
    }

    // no-engineering-boilerplate (conversational turns only)
    if (turn.kind === 'ask' && turn.status === 'completed') {
      const hit = TASK_PROFILE_MARKERS.find((marker) => texts.some((t) => t.includes(marker)));
      if (hit === undefined) {
        push(`no-engineering-boilerplate@${i}`, 'pass', 'no task protocol/state marker in request material');
      } else {
        push(
          `no-engineering-boilerplate@${i}`,
          'fail',
          `conversational turn carries engineering boilerplate marker ${JSON.stringify(hit)}`,
        );
      }
    }

    // cross-turn-reference: turn N must carry turn N-2's reply content
    if (i >= 2 && turn.status === 'completed') {
      const ref = recorded.turns[i - 2];
      const refText = String(ref?.assistant_text ?? '');
      if (refText.trim().length < 40) {
        push(`cross-turn-reference@${i}`, 'review', `turn ${i - 2} has no usable reply text to reference`);
      } else {
        let candidates = distinctiveTokens(refText, userPrompts);
        if (candidates.length === 0) candidates = distinctiveTokens(refText, new Set());
        if (candidates.length === 0) {
          push(`cross-turn-reference@${i}`, 'review', `turn ${i - 2} reply has no distinctive tokens`);
        } else {
          const need = Math.min(2, candidates.length);
          const joined = texts.join('\n');
          const matches = candidates.filter((token) => joined.includes(token)).length;
          if (matches >= need) {
            push(`cross-turn-reference@${i}`, 'pass', `${matches}/${candidates.length} reply tokens present in request`);
          } else {
            push(
              `cross-turn-reference@${i}`,
              'fail',
              `request carries only ${matches}/${need} required tokens of turn ${i - 2}'s reply (history/context not captured)`,
            );
          }
        }
      }
    }

    // engineering-state-only-at-escalation: no task state before the esc turn
    if (i < esc && (turn.status === 'completed' || turn.status === 'failed')) {
      const before = turn.tasks_before ?? 0;
      const after = turn.tasks_after ?? 0;
      const verifOk = verifyShapeEmpty(turn.verification_before);
      if (before === 0 && after === 0 && verifOk) {
        push(`engineering-state-only-at-escalation@${i}`, 'pass', 'no task ledger / verification state');
      } else {
        push(
          `engineering-state-only-at-escalation@${i}`,
          'fail',
          `pre-escalation engineering state visible: tasks_before=${before} tasks_after=${after} verification_empty=${verifOk}`,
        );
      }
    }

    // escalation-established: only the escalation turn builds engineering state
    if (i === esc) {
      if (turn.status === 'skipped-cost-cap') {
        push(`escalation-established@${i}`, 'review', 'escalation turn never ran (cost cap)');
      } else if (turn.status !== 'completed' && turn.status !== 'aborted-cost-bound') {
        push(`escalation-established@${i}`, 'fail', `escalation turn ended '${turn.status}' — engineering state never established`);
      } else {
        const tasks = turn.tasks_after ?? 0;
        const taskProfile = texts.some((t) => t.includes('## Working protocol'));
        if (tasks >= 1 && taskProfile) {
          push(`escalation-established@${i}`, 'pass', `task ledger entries=${tasks}; task-profile request recorded`);
        } else {
          push(
            `escalation-established@${i}`,
            'fail',
            `escalation did not establish engineering state (tasks_after=${tasks}, task_profile_request=${taskProfile})`,
          );
        }
      }
    }

    // Human-review hooks (never fail the run).
    if (turn.kind === 'ask' && turn.status === 'completed') {
      if (turn.id === 'pronoun-follow-up') {
        reviewItems.push(`turn ${i} (${turn.id}): human judges whether the pronoun was resolved against turn ${i - 2}'s actual content`);
      }
      if (turn.id === 'instruction-reversal') {
        reviewItems.push(`turn ${i} (${turn.id}): human judges that the assistant explained WITHOUT claiming an edit (no fix applied)`);
      }
      if (turn.has_attachment) {
        reviewItems.push(`turn ${i} (${turn.id}): human inspects the recorded request for the attached file's material`);
      }
      if (texts.length > 0) {
        reviewItems.push(`turn ${i} (${turn.id}): human spot-check of history/context captured in the recorded request`);
      }
    }
    if (turn.kind === 'task' && turn.status === 'completed') {
      reviewItems.push(`turn ${i} (${turn.id}): human judges the assistant's change summary/claims against the recorded request`);
    }
  }

  const total = (recorded.turns ?? []).reduce((n, t) => n + (t.requests?.length ?? 0), 0);
  const cap = recorded.max_requests ?? 20;
  if (total <= cap) {
    push('cost-cap', 'pass', `${total}/${cap} provider requests`);
  } else {
    push('cost-cap', 'fail', `${total} provider requests exceed the ${cap} cap`);
  }

  return { gates, review_items: reviewItems };
}

// ------------------------------------------------------------- selftest

function runSelftest() {
  if (!existsSync(GATE_FIXTURE_DIR)) fail(`gate fixture dir missing: ${GATE_FIXTURE_DIR}`);
  const cases = readFileSync(join(GATE_FIXTURE_DIR, 'index.json'), 'utf8');
  const { fixtures } = JSON.parse(cases);
  let failures = 0;
  for (const name of fixtures) {
    const fixture = JSON.parse(readFileSync(join(GATE_FIXTURE_DIR, name), 'utf8'));
    const { gates, review_items } = evaluateGates(fixture.recorded);
    const failSet = gates.filter((g) => g.status === 'fail').map((g) => g.id).sort();
    const reviewSet = gates.filter((g) => g.status === 'review').map((g) => g.id).sort();
    const expectFail = [...fixture.expect.fail].sort();
    const expectReview = [...fixture.expect.review].sort();
    const ok =
      JSON.stringify(failSet) === JSON.stringify(expectFail) &&
      JSON.stringify(reviewSet) === JSON.stringify(expectReview) &&
      (fixture.expect.review_items_min === undefined ||
        review_items.length >= fixture.expect.review_items_min);
    if (ok) {
      console.log(`selftest ok: ${name} (fail=${JSON.stringify(failSet)})`);
    } else {
      failures += 1;
      console.error(
        `selftest FAIL: ${name}\n  expected fail=${JSON.stringify(expectFail)} review=${JSON.stringify(expectReview)}` +
          `\n  actual   fail=${JSON.stringify(failSet)} review=${JSON.stringify(reviewSet)} review_items=${review_items.length}`,
      );
    }
  }
  if (failures > 0) {
    console.error(`run-evals gate selftest: FAIL (${failures}/${fixtures.length})`);
    process.exit(1);
  }
  console.log(`run-evals gate selftest: PASS (${fixtures.length} planted fixtures, violations rejected, review items never fail)`);
}

// ---------------------------------------------------- fixtures mode (1M probe)

function runFixturesProbe() {
  console.log('run-evals: launching the #[ignore]d 1M-line fixture scale probe (explicit manual-lane script)...');
  const child = spawn(
    'cargo',
    ['test', '-p', 'faktor-tests-eval', '--release', '--test', 'fixtures_scale', '--', '--ignored', '--nocapture'],
    { cwd: ROOT, stdio: 'inherit' },
  );
  child.on('exit', (code) => process.exit(code ?? 1));
}

// ------------------------------------------------------- recording proxy

function startRecordingProxy(maxRequests) {
  const records = [];
  const state = { rejected: 0 };
  const server = http.createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const body = Buffer.concat(chunks).toString('utf8');
      if (records.length >= maxRequests) {
        // Hard cost backstop: past the cap the proxy refuses to forward, so
        // even a runaway tool loop cannot spend more completions.
        state.rejected += 1;
        res.writeHead(503, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ error: { message: 'eval completion cost cap reached' } }));
        return;
      }
      records.push({ at: new Date().toISOString(), path: req.url, body });
      const upstream = https.request(
        {
          host: UPSTREAM_HOST,
          port: 443,
          path: req.url,
          method: req.method,
          headers: { ...req.headers, host: UPSTREAM_HOST },
          // No pooled keep-alive sockets: a stale pooled connection that the
          // upstream closed mid-stream turns into intermittent ECONNRESET
          // 502s through the proxy and false turn failures.
          agent: false,
        },
        (up) => {
          res.writeHead(up.statusCode ?? 502, up.headers);
          up.pipe(res);
        },
      );
      upstream.on('error', (e) => {
        if (!res.headersSent) res.writeHead(502, { 'content-type': 'application/json' });
        res.end(JSON.stringify({ error: { message: `proxy upstream: ${e.message}` } }));
      });
      if (body.length > 0) upstream.write(body);
      upstream.end();
    });
  });
  return new Promise((res) => {
    server.listen(0, '127.0.0.1', () => {
      res({ server, port: server.address().port, records, state });
    });
  });
}

// ------------------------------------------------------------ daemon drive

async function startDaemon({ bin, dataDir, proxyPort, providerEnv }) {
  const password = randomUUID().replace(/-/g, '') + randomUUID().replace(/-/g, '');
  const config = {
    config_version: 1,
    model: 'deepseek-chat',
    providers: [
      {
        kind: 'open_ai',
        id: 'deepseek',
        base_url: `http://127.0.0.1:${proxyPort}/v1`,
        api: 'chat',
        allow_loopback: true,
        api_key_env: 'FAKTOR_DEEPSEEK_API_KEY',
        models: ['deepseek-chat'],
        quality: {
          coding_reliability: 90,
          context_reliability: 90,
          tool_reliability: 90,
          reasoning_reliability: 90,
        },
        pricing: { pricing_ceiling_micro_usd_per_million_tokens: 2000000 },
      },
    ],
    routing_mode: { pinned: { provider: 'deepseek', model: 'deepseek-chat' } },
    // Throwaway data-dir on possibly-nearly-full developer volumes: the
    // default 5%-of-capacity reserve can exceed free space and refuse even
    // sqlite_wal admission. The safe-range floor (1% + 64MiB) keeps the
    // governor real while admitting the eval's tiny workload.
    storage: {
      reserve_permille: 10,
      reserve_bytes: 67108864,
    },
    sandbox: {
      network: [`http://127.0.0.1:${proxyPort}`],
      network_guarantee: 'required',
      home: 'scratch',
    },
  };
  mkdirSync(dataDir, { recursive: true });
  writeFileSync(join(dataDir, 'faktor-plus.json'), JSON.stringify(config, null, 2));
  const child = spawn(bin, ['serve', '--port', '0', '--data-dir', dataDir], {
    env: { ...process.env, ...providerEnv, FAKTOR_SERVER_PASSWORD: password },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (d) => {
    stderr = (stderr + String(d)).slice(-8000);
  });
  const port = await new Promise((res, rej) => {
    let out = '';
    const timer = setTimeout(() => rej(new Error(`daemon did not start: ${out.slice(-800)}`)), 60_000);
    child.stdout.on('data', (d) => {
      out += String(d);
      const m = /listening on http:\/\/127\.0\.0\.1:(\d+)/.exec(out);
      if (m) {
        clearTimeout(timer);
        res(Number(m[1]));
      }
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      rej(new Error(`daemon exited early (${code}): ${out.slice(-800)} stderr=${stderr.slice(-600)}`));
    });
  });
  return {
    port,
    password,
    child,
    stderrTail: () => stderr,
    stop() {
      try {
        child.kill('SIGKILL');
      } catch {
        /* gone */
      }
    },
  };
}

async function waitReady(daemon) {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const r = await httpJson({
      port: daemon.port,
      method: 'GET',
      path: '/native/ready',
      token: daemon.password,
    }).catch(() => null);
    if (r !== null && r.status === 200 && r.json?.ready === true) return;
    await sleep(250);
  }
  fail(`daemon never became ready: ${daemon.stderrTail().slice(-500)}`);
}

function subscribeTurns(daemon, sessionId) {
  // SSE of durable turn lifecycle frames: {turn, opId, status} — the
  // documented completion authority.
  const events = [];
  const req = http.request(
    {
      host: '127.0.0.1',
      port: daemon.port,
      method: 'GET',
      path: `/native/session/${sessionId}/events?after=0`,
      headers: { authorization: `Bearer ${daemon.password}`, accept: 'text/event-stream' },
    },
    (res) => {
      if (res.statusCode !== 200) {
        res.resume();
        return;
      }
      let buffer = '';
      res.setEncoding('utf8');
      res.on('data', (chunk) => {
        buffer += chunk;
        for (;;) {
          const split = buffer.indexOf('\n\n');
          if (split < 0) break;
          const block = buffer.slice(0, split);
          buffer = buffer.slice(split + 2);
          let event = 'message';
          const dataLines = [];
          for (const line of block.split('\n')) {
            if (line.startsWith('event: ')) event = line.slice(7).trim();
            if (line.startsWith('data: ')) dataLines.push(line.slice(6));
          }
          events.push({ event, data: dataLines.join('\n') });
        }
      });
    },
  );
  req.on('error', () => {
    /* dropped connection is tolerated; polling below still completes */
  });
  req.end();
  return { events, close: () => req.destroy() };
}

async function assistantMessages(daemon, sessionId) {
  const page = await httpJson({
    port: daemon.port,
    method: 'GET',
    path: `/native/messages?session=${sessionId}&limit=50`,
    token: daemon.password,
  });
  const messages = page.json?.messages ?? [];
  return messages
    .filter((m) => m.role === 'assistant')
    .map((m) =>
      (m.parts ?? []).map((p) => (typeof p?.data?.text === 'string' ? p.data.text : '')).join(''),
    );
}

async function countTasks(daemon, sessionId) {
  const r = await httpJson({
    port: daemon.port,
    method: 'GET',
    path: `/native/session/${sessionId}/tasks`,
    token: daemon.password,
  }).catch(() => null);
  if (r === null || r.status !== 200) return null;
  if (Array.isArray(r.json)) return r.json.length;
  for (const key of ['tasks', 'items', 'rows', 'runs']) {
    if (Array.isArray(r.json?.[key])) return r.json[key].length;
  }
  return null;
}

async function verificationShape(daemon, sessionId) {
  const r = await httpJson({
    port: daemon.port,
    method: 'GET',
    path: `/native/session/${sessionId}/verification`,
    token: daemon.password,
  }).catch(() => null);
  if (r === null || r.status !== 200) return null;
  return r.json;
}

async function waitForTurnEnd({ daemon, sessionId, opId, timeoutMs }) {
  // Completion authority: the durable turn record of THIS prompt's op id
  // reaching a terminal status. Waiting on the whole list is wrong — a
  // queued turn is not active yet, and an earlier completed turn must never
  // be mistaken for this one's end.
  const deadline = Date.now() + timeoutMs;
  const terminal = new Set(['completed', 'failed', 'cancelled']);
  while (Date.now() < deadline) {
    const r = await httpJson({
      port: daemon.port,
      method: 'GET',
      path: `/native/session/${sessionId}/turns`,
      token: daemon.password,
    }).catch(() => null);
    if (r !== null && r.status === 200 && Array.isArray(r.json) && r.json.length > 0) {
      const mine = opId ? r.json.find((t) => t.opId === opId) : undefined;
      if (mine !== undefined && terminal.has(mine.status)) {
        return mine.status;
      }
      if (mine === undefined && opId === null) {
        // No op id from admission: fall back to "no in-flight turn at all".
        const inflight = r.json.some((t) => !terminal.has(t.status));
        if (!inflight) {
          return r.json[r.json.length - 1].status;
        }
      }
    }
    await sleep(1_000);
  }
  return 'timeout';
}

async function waitForAssistantDelta(daemon, sessionId, beforeCount, timeoutMs) {
  // Durable assistant text of THIS turn: poll until the transcript gains
  // whole NEW assistant messages past the pre-prompt snapshot (the store
  // may finalize text just after the turn record turns terminal).
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const now = await assistantMessages(daemon, sessionId);
    if (now.length > beforeCount) return now.slice(beforeCount).join('');
    await sleep(500);
  }
  return '';
}

async function uploadAttachment(daemon, sessionId, bytes, mime, filename) {
  const r = await httpJson({
    port: daemon.port,
    method: 'POST',
    path: `/native/session/${sessionId}/attachments`,
    token: daemon.password,
    body: { mime, filename, data_base64: bytes.toString('base64') },
  });
  if (r.status !== 200 || typeof r.json?.digest !== 'string') return null;
  return { digest: r.json.digest, mime: r.json.mime, filename: r.json.filename ?? null, size: r.json.size };
}

// --------------------------------------------------------------- redaction

function redact(value, secrets) {
  if (typeof value === 'string') {
    let out = value;
    for (const secret of secrets) {
      if (secret && secret.length >= 8) out = out.split(secret).join('[redacted]');
    }
    return out;
  }
  if (Array.isArray(value)) return value.map((v) => redact(v, secrets));
  if (value !== null && typeof value === 'object') {
    const out = {};
    for (const [k, v] of Object.entries(value)) {
      if (/api[-_]?key|authorization|password|secret|bearer|token/i.test(k)) {
        out[k] = '[redacted]';
      } else {
        out[k] = redact(v, secrets);
      }
    }
    return out;
  }
  return value;
}

// ------------------------------------------------------------ suite definition

function buildSuite(manifest) {
  return [
    { id: 'greeting', kind: 'ask', prompt: 'Hello! Who are you, and what can you help me with in this repository? One short paragraph.' },
    {
      id: 'conceptual-explanation',
      kind: 'ask',
      prompt: 'Explain in two paragraphs how a content-addressable store works and why an agent runtime would use one. Two paragraphs only.',
    },
    {
      id: 'code-explanation',
      kind: 'ask',
      attach: 'rust/telemetry_core/src/lib.rs',
      prompt:
        'Explain what the compute_checksum function in rust/telemetry_core/src/lib.rs does and how ingest_event uses it. One short paragraph.',
    },
    { id: 'pronoun-follow-up', kind: 'ask', prompt: null /* built from turn 1 */ },
    {
      id: 'instruction-reversal',
      kind: 'ask',
      prompt:
        'Suppose someone reported that the FNV offset basis in rust/telemetry_core/src/lib.rs looks wrong. Do NOT fix anything yet — just explain what would actually be wrong and where it would show up.',
    },
    {
      id: 'repository-lookup',
      kind: 'ask',
      prompt:
        'Where is compute_checksum defined, and which files in this repository reference faktor_eval_crossref_core? Give exact paths.',
    },
    {
      id: 'compare',
      kind: 'ask',
      prompt:
        'Compare the checksum approach in rust/telemetry_core/src/lib.rs with the one in ts/telemetry-edge/src/ingest.ts. Two sentences.',
    },
    {
      id: 'return-to-earlier-topic',
      kind: 'ask',
      prompt:
        'Going back to content-addressable stores from earlier — how would you apply that idea to the event log in this repository? One paragraph.',
    },
    {
      id: 'escalation',
      kind: 'task',
      prompt:
        'Now implement it: add a one-line doc comment to compute_checksum in rust/telemetry_core/src/lib.rs noting that event payloads can be content-addressed. Change nothing else.',
    },
    {
      id: 'task-sequence',
      kind: 'task',
      prompt: 'Summarize exactly what you changed and which verification you ran, in two sentences.',
    },
  ];
}

function pronounPrompt(assistantText1) {
  const paragraphs = String(assistantText1 ?? '').split(/\n\s*\n/);
  const second = paragraphs[1] ?? paragraphs.join(' ');
  const candidates = distinctiveTokens(second, new Set());
  const term = candidates[0] ?? null;
  if (term === null) {
    return { prompt: 'In your previous answer you referred to "that mechanism" earlier on. What exactly did you mean? One sentence with an example.', extracted: null };
  }
  return {
    prompt: `In your second paragraph you mentioned "${term}". What did you mean by that? Explain with a concrete example in one short paragraph.`,
    extracted: term,
  };
}

// ---------------------------------------------------------------- main run

async function runConversation() {
  const providerEnv = readProviderEnv();
  if (providerEnv === null) {
    skip(`provider env file not found or empty: ${ENV_FILE} (KEY=VALUE expected) — real-provider eval not run`);
  }
  const bin = resolveBinary();
  if (bin === null) {
    skip('no faktor daemon binary found (pass --bin or build target/debug/faktor-cli)');
  }
  const maxRequests = Number(argValue('max-requests', '20'));
  if (!Number.isFinite(maxRequests) || maxRequests < 10 || maxRequests > 20) {
    fail('--max-requests must be within [10, 20] (suite cost bound)');
  }

  // 1. Polyglot fixture workspace (deterministic generator, committed code).
  //    If the workspace cannot currently build (shared-checkout WIP), a
  //    previously generated fixture with the SAME seed and schema is
  //    byte-identical by the generator's determinism invariant, so reuse it
  //    instead of failing the run.
  console.log(`run-evals: generating polyglot fixture workspace at ${POLYGLOT_DIR}`);
  const reuseValidFixture = () => {
    try {
      const existing = JSON.parse(readFileSync(join(POLYGLOT_DIR, 'manifest.json'), 'utf8'));
      if (
        existing.schema === 'faktor-eval-corpus/v1' &&
        existing.kind === 'polyglot' &&
        existing.seed === 42 &&
        Array.isArray(existing.files) &&
        existing.files.length > 0
      ) {
        return existing;
      }
    } catch {
      /* no usable fixture */
    }
    return null;
  };
  const gen = spawn(
    'cargo',
    ['run', '-q', '-p', 'faktor-tests-eval', '--bin', 'eval_fixtures', '--', 'polyglot', '--root', POLYGLOT_DIR, '--seed', '42'],
    { cwd: ROOT, stdio: ['ignore', 'pipe', 'inherit'] },
  );
  const generated = await new Promise((res) => {
    let out = '';
    gen.stdout.on('data', (d) => (out += String(d)));
    gen.on('exit', (code) => {
      if (code !== 0) return res(null);
      try {
        res(JSON.parse(out));
      } catch {
        res(null);
      }
    });
  });
  const manifest = generated ?? reuseValidFixture();
  if (manifest === null) {
    fail('fixture generation failed and no prior seed-42 fixture exists under target/eval-fixtures');
  }
  if (generated === null) {
    console.log('run-evals: WARNING — workspace build failed; reusing the existing byte-identical seed-42 fixture (generator determinism invariant)');
  }
  console.log(`run-evals: fixture ready (${manifest.files.length} files, ${manifest.total_lines} lines)`);

  // 2. Recording proxy + throwaway daemon (temp data-dir).
  const proxy = await startRecordingProxy(maxRequests);
  const runDir = mkdtempSync(join(tmpdir(), 'faktor-eval-'));
  const dataDir = join(runDir, 'data');
  console.log(`run-evals: recording proxy on 127.0.0.1:${proxy.port} -> ${UPSTREAM_HOST}; data-dir ${dataDir}`);
  const daemon = await startDaemon({ bin, dataDir, proxyPort: proxy.port, providerEnv });
  const secrets = [
    providerEnv.FAKTOR_DEEPSEEK_API_KEY ?? '',
    daemon.password,
  ];
  let sessionId = null;
  let exitCode = 0;
  try {
    await waitReady(daemon);

    // 3. Session over the fixture workspace (read-only conversation scope).
    const created = await httpJson({
      port: daemon.port,
      method: 'POST',
      path: '/native/session',
      token: daemon.password,
      body: {
        provider: 'deepseek',
        model: 'deepseek-chat',
        workspace: POLYGLOT_DIR,
        title: 'conversation-quality eval',
      },
    });
    if (created.status !== 200 || typeof created.json?.id !== 'string') {
      fail(`session create: ${created.status} ${created.text.slice(0, 300)} stderr=${daemon.stderrTail().slice(-400)}`);
    }
    sessionId = created.json.id;
    const events = subscribeTurns(daemon, sessionId);
    // Scripted human in the chair: a turn that reaches a read/search tool
    // parks on a durable permission prompt (`permission 5 not resolved
    // within 60000ms` otherwise kills the turn). Poll and ALLOW every
    // pending permission of THIS session — every resolution is recorded in
    // the report for human review.
    const permissionsResolved = [];
    const permissionTimer = setInterval(async () => {
      const list = await httpJson({
        port: daemon.port,
        method: 'GET',
        path: `/native/permissions?session=${encodeURIComponent(sessionId)}`,
        token: daemon.password,
      }).catch(() => null);
      if (list === null || list.status !== 200) return;
      const pending = Array.isArray(list.json)
        ? list.json
        : Array.isArray(list.json?.permissions)
          ? list.json.permissions
          : Array.isArray(list.json?.items)
            ? list.json.items
            : [];
      for (const p of pending) {
        if (p.session_id !== sessionId) continue;
        const reply = await httpJson({
          port: daemon.port,
          method: 'POST',
          path: '/native/permission/reply',
          token: daemon.password,
          body: { session_id: sessionId, permission_id: String(p.id), decision: 'allow' },
        }).catch(() => null);
        if (reply !== null && reply.status === 200) {
          permissionsResolved.push({ id: String(p.id), capability: p.capability, at: new Date().toISOString() });
          console.log(`run-evals: allowed permission ${p.id} (${p.capability})`);
        }
      }
    }, 1_000);
    permissionTimer.unref?.();

    const suite = buildSuite(manifest);
    const escalationIndex = suite.findIndex((t) => t.kind === 'task');
    const recorded = { max_requests: maxRequests, escalation_index: escalationIndex, turns: [] };
    const attachMarker = manifest.crossref.attachment_marker;
    let abortedForCost = false;

    for (let i = 0; i < suite.length; i += 1) {
      const spec = suite[i];
      if (abortedForCost || proxy.records.length >= maxRequests - 2) {
        recorded.turns.push({
          id: spec.id,
          kind: spec.kind,
          status: 'skipped-cost-cap',
          requests: [],
          assistant_text: '',
          tasks_before: 0,
          tasks_after: 0,
          user_prompt: spec.prompt ?? '',
        });
        continue;
      }

      // Pronoun turn is built from turn 1's reply.
      let prompt = spec.prompt;
      if (spec.id === 'pronoun-follow-up') {
        const built = pronounPrompt(recorded.turns[1]?.assistant_text ?? '');
        prompt = built.prompt;
        if (built.extracted === null) {
          console.warn('run-evals: pronoun term extraction fell back (review item recorded)');
        }
      }

      const tasksBefore = (await countTasks(daemon, sessionId)) ?? 0;
      const verificationBefore = await verificationShape(daemon, sessionId);
      const mark = proxy.records.length;
      const assistantBeforeCount = (await assistantMessages(daemon, sessionId)).length;

      let attachments;
      if (spec.attach) {
        const bytes = readFileSync(join(POLYGLOT_DIR, spec.attach));
        const uploaded = await uploadAttachment(daemon, sessionId, bytes, 'text/x-rust', spec.attach.split('/').pop());
        attachments = uploaded ? [uploaded] : undefined;
        if (uploaded === null) console.warn('run-evals: attachment upload refused; gate will mark review');
      }

      const promptBody = {
        session_id: sessionId,
        submission_id: randomUUID(),
        prompt,
        execution: spec.kind === 'task' ? 'task' : 'ask',
        ...(attachments ? { attachments } : {} ),
      };
      const accepted = await httpJson({
        port: daemon.port,
        method: 'POST',
        path: `/native/session/${sessionId}/prompt`,
        token: daemon.password,
        body: promptBody,
      });
      if (accepted.status !== 200) {
        recorded.turns.push({
          id: spec.id,
          kind: spec.kind,
          status: `failed:prompt-${accepted.status}`,
          requests: proxy.records.slice(mark).map((r) => r.body),
          assistant_text: '',
          tasks_before: tasksBefore,
          tasks_after: (await countTasks(daemon, sessionId)) ?? 0,
          verification_before: verificationBefore,
          user_prompt: prompt,
          has_attachment: Boolean(attachments),
        });
        continue;
      }
      // Completion authority rides the accepted prompt's op id; a null op id
      // falls back to "no in-flight turn" inside waitForTurnEnd. A receipt
      // that was not accepted is a typed admission failure, not a hang.
      const opId = typeof accepted.json?.op_id === 'string' ? accepted.json.op_id : null;
      if (accepted.json?.accepted === false) {
        recorded.turns.push({
          id: spec.id,
          kind: spec.kind,
          status: 'failed:admission',
          requests: proxy.records.slice(mark).map((r) => r.body),
          assistant_text: '',
          tasks_before: tasksBefore,
          tasks_after: (await countTasks(daemon, sessionId)) ?? 0,
          verification_before: verificationBefore,
          user_prompt: prompt,
          has_attachment: Boolean(attachments),
        });
        continue;
      }

      // For the escalation turn: once engineering state IS established and
      // the task profile was recorded, a still-running tool loop gets a
      // bounded grace window and is then aborted (cost discipline).
      let status = 'completed';
      if (spec.kind === 'task' && i === escalationIndex) {
        const stateDeadline = Date.now() + 90_000;
        let established = false;
        while (Date.now() < stateDeadline) {
          const tasks = await countTasks(daemon, sessionId);
          const requestSoFar = proxy.records.slice(mark).map((r) => r.body);
          established =
            (tasks ?? 0) >= 1 && requestSoFar.some((t) => t.includes('## Working protocol'));
          if (established) break;
          await sleep(2_000);
        }
        const endStatus = await waitForTurnEnd({ daemon, sessionId, opId, timeoutMs: 60_000 });
        if (endStatus === 'timeout' && established) {
          // Abort the tool loop; state is proven, budget must hold.
          await httpJson({
            port: daemon.port,
            method: 'POST',
            path: `/native/session/${sessionId}/abort`,
            token: daemon.password,
            body: { session_id: sessionId },
          });
          status = 'aborted-cost-bound';
        } else if (endStatus !== 'completed') {
          status = endStatus === 'timeout' ? 'failed:timeout' : endStatus;
        }
      } else {
        const endStatus = await waitForTurnEnd({
          daemon,
          sessionId,
          opId,
          timeoutMs: spec.kind === 'task' ? 240_000 : 180_000,
        });
        if (endStatus === 'completed') status = 'completed';
        else if (endStatus === 'timeout') status = 'failed:timeout';
        else status = endStatus;
      }

      const assistantTextForTurn = await waitForAssistantDelta(daemon, sessionId, assistantBeforeCount, 15_000);
      const tasksAfter = (await countTasks(daemon, sessionId)) ?? 0;
      const requests = proxy.records.slice(mark).map((r) => r.body);

      recorded.turns.push({
        id: spec.id,
        kind: spec.kind,
        status,
        requests,
        assistant_text: assistantTextForTurn.trim(),
        tasks_before: tasksBefore,
        tasks_after: tasksAfter,
        verification_before: verificationBefore,
        user_prompt: prompt,
        has_attachment: Boolean(attachments),
      });
      console.log(
        `run-evals: turn ${i} ${spec.id} -> ${status} (requests so far: ${proxy.records.length}/${maxRequests}, tasks_after=${tasksAfter})`,
      );

      if (spec.id === 'code-explanation' && status === 'completed') {
        const markerSeen = requests.some((t) => t.includes(attachMarker));
        console.log(markerSeen
          ? 'run-evals: attachment material present in the recorded provider request'
          : 'run-evals: attachment material NOT found in the recorded request (review item)');
      }
      if (proxy.records.length >= maxRequests - 1) abortedForCost = true;
    }

    events.close();
    clearInterval(permissionTimer);

    // 4. Gates over the normalized recording.
    const { gates, review_items } = evaluateGates(recorded);
    for (const p of permissionsResolved) {
      review_items.push(`permission ${p.id} (${p.capability}) auto-allowed by the scripted run — human confirms the capability was expected`);
    }
    const failCount = gates.filter((g) => g.status === 'fail').length;
    const reviewCount = gates.filter((g) => g.status === 'review').length;
    const passCount = gates.filter((g) => g.status === 'pass').length;

    // 5. Reviewable report (requests redacted of secrets).
    const report = redact(
      {
        schema: 'faktor-eval-report/v1',
        kind: 'run',
        generated_at: new Date().toISOString(),
        suite: 'conversation-quality/v1',
        provider: { id: 'deepseek', model: 'deepseek-chat', upstream: UPSTREAM_HOST },
        daemon: { binary: bin, data_dir: '[temp]', stderr_tail: daemon.stderrTail().slice(-4000) },
        workspace: 'target/eval-fixtures/polyglot (seed 42)',
        budget: { max_provider_requests: maxRequests, provider_requests: proxy.records.length, rejected_past_cap: proxy.state.rejected },
        permissions_resolved: permissionsResolved,
        turns: recorded.turns.map((t, index) => ({
          index,
          id: t.id,
          kind: t.kind,
          status: t.status,
          tasks_before: t.tasks_before,
          tasks_after: t.tasks_after,
          assistant_excerpt: String(t.assistant_text ?? '').slice(0, 600),
          gates: gates.filter((g) => g.id.endsWith(`@${index}`)),
          provider_requests: (t.requests ?? []).map((body, rIndex) => {
            let parsed = body;
            try {
              parsed = JSON.parse(body);
            } catch {
              /* raw */
            }
            const serialized = JSON.stringify(parsed);
            return {
              index: rIndex,
              bytes: serialized.length,
              truncated: serialized.length > 262_144,
              body: serialized.length > 262_144 ? `${serialized.slice(0, 262_144)}…[truncated]` : parsed,
            };
          }),
        })),
        global_gates: gates.filter((g) => !/@\d+$/.test(g.id)),
        review_items,
        summary: {
          turns: recorded.turns.length,
          provider_requests: proxy.records.length,
          gates_pass: passCount,
          gates_fail: failCount,
          gates_review: reviewCount,
          assertion_failures: failCount,
        },
      },
      secrets,
    );

    mkdirSync(REPORT_DIR, { recursive: true });
    const stamp = report.generated_at.replace(/[:.]/g, '-');
    const reportPath = join(REPORT_DIR, `${stamp}.report.json`);
    writeFileSync(reportPath, JSON.stringify(report, null, 2));

    console.log('-----------------------------------------------');
    console.log(`turns: ${recorded.turns.length}  provider requests: ${proxy.records.length}/${maxRequests} (rejected past cap: ${proxy.state.rejected})`);
    console.log(`gates: pass=${passCount} fail=${failCount} review=${reviewCount}`);
    for (const g of gates.filter((x) => x.status !== 'pass')) {
      console.log(`  [${g.status}] ${g.id}: ${g.detail}`);
    }
    console.log(`report: ${reportPath}`);
    if (failCount > 0) {
      console.error(`run-evals: FAIL (${failCount} assertion failure(s); review items never fail the run)`);
      exitCode = 1;
    } else {
      console.log(`run-evals: PASS (0 assertion failures; ${reviewCount} review gates, ${review_items.length} review items for human judgement)`);
    }
  } catch (error) {
    console.error(`daemon stderr tail: ${daemon.stderrTail().slice(-1200)}`);
    fail(error instanceof Error ? error.message : String(error));
  } finally {
    daemon.stop();
    proxy.server.close();
    rmSync(runDir, { recursive: true, force: true });
  }
  process.exit(exitCode);
}

// ------------------------------------------------------------------ entry

const args = process.argv.slice(2);
if (args.includes('--selftest')) {
  runSelftest();
} else if (args.includes('fixtures')) {
  runFixturesProbe();
} else if (args.length === 0 || args.includes('conversation')) {
  await runConversation();
} else {
  console.error('usage: node scripts/run-evals.mjs [conversation|fixtures] [--selftest] [--bin PATH] [--max-requests N]');
  process.exit(2);
}
