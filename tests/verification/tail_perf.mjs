#!/usr/bin/env node
// Independent tail-latency / bounded-memory probe (new angle; not a rerun of
// the standing route probe). It drives the REAL daemon with a mixed steady
// workload at fixed concurrency, then a hostile burst, and records p50/p95/
// p99/max, typed statuses, and resident-memory growth.
import { execFileSync } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { cliBinary, request, startDaemon } from '../programme/probes/routes.mjs';
import { sqliteExec } from './sqlite_exec.mjs';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const OUT = resolve(ROOT, 'target/certification/verification/tail-perf.json');
const CONCURRENCY = Number(process.env.TAIL_CONCURRENCY ?? 32);
const DURATION_MS = Number(process.env.TAIL_DURATION_MS ?? 2500);
const P95_BOUND_MS = Number(process.env.TAIL_P95_BOUND_MS ?? 500);
const MAX_BOUND_MS = Number(process.env.TAIL_MAX_BOUND_MS ?? 3000);
const SEARCH_P95_BOUND_MS = Number(process.env.TAIL_SEARCH_P95_BOUND_MS ?? 2000);
const RSS_GROWTH_BOUND_BYTES = Number(process.env.TAIL_RSS_BOUND_BYTES ?? 64 * 1024 * 1024);

function rss(pid) {
  try {
    return Number(execFileSync('ps', ['-o', 'rss=', '-p', String(pid)], { encoding: 'utf8' }).trim()) * 1024;
  } catch {
    return null;
  }
}

function percentile(sorted, p) {
  if (sorted.length === 0) return null;
  const at = Math.min(sorted.length - 1, Math.max(0, Math.ceil((p / 100) * sorted.length) - 1));
  return sorted[at];
}

export async function probeTail({ report = false } = {}) {
  const binary = cliBinary();
  if (binary === null) return { status: 'deferred', reason: 'no faktor-cli binary' };
  const tmp = mkdtempSync(join(tmpdir(), 'faktor-tail-probe-'));
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
          api_key_env: 'TAIL_PROBE_KEY',
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
      title: 'tail probe',
    });
    if (created.status !== 200) throw new Error(`tail probe session ${created.status}`);
    const session = String(created.json.id);
    const paths = [
      ['health', '/native/health'],
      ['ready', '/native/ready'],
      ['sessions', '/native/sessions?limit=50'],
      ['messages', `/native/messages?session=${session}&limit=50`],
      ['events', `/native/events?session=${session}&after=0&limit=50`],
      ['usage', `/native/session/${session}/usage`],
      ['outcomes', '/native/outcomes?limit=50'],
      ['permissions', '/native/permissions'],
    ];
    const beforeRss = rss(daemon.child.pid);
    const latencies = [];
    const statuses = {};
    const deadline = Date.now() + DURATION_MS;
    let cursor = 0;
    const worker = async () => {
      while (Date.now() < deadline) {
        const [name, path] = paths[cursor % paths.length];
        cursor += 1;
        const started = performance.now();
        const result = await request(base, token, 'GET', path, undefined, MAX_BOUND_MS);
        const elapsed = performance.now() - started;
        latencies.push(elapsed);
        statuses[name] ??= {};
        statuses[name][result.status ?? 'aborted'] = (statuses[name][result.status ?? 'aborted'] ?? 0) + 1;
      }
    };
    await Promise.all(Array.from({ length: CONCURRENCY }, worker));
    const sorted = [...latencies].sort((a, b) => a - b);
    const afterSteadyRss = rss(daemon.child.pid);

    const failures = [];
    // ---- large-transcript scale phase (50k rows), the changed read paths.
    // Seed durable rows directly (the probe owns the same tmp data dir) so
    // the measurement is the daemon's SQL/keyset path, not HTTP write cost.
    const storeDb = join(data, 'store', 'faktor-plus.db');
    const scale_driver = await sqliteExec(storeDb, `
      BEGIN;
      WITH RECURSIVE c(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM c WHERE x < 50000)
        INSERT OR IGNORE INTO message(session_id, seq, role, data, created_ms)
        SELECT ${Number(session)}, x, 'user', json_object('text', 'message ' || x), 0 FROM c;
      WITH RECURSIVE c(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM c WHERE x < 50000)
        INSERT INTO part(message_id, kind, data, created_ms)
        SELECT m.id, 'text', json_object('text', 'part ' || m.seq), 0
        FROM message m JOIN c ON c.x = m.seq WHERE m.session_id = ${Number(session)};
      WITH RECURSIVE c(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM c WHERE x < 50000)
        INSERT OR IGNORE INTO event(seq, session_id, kind, state, ts_ms, payload)
        SELECT x, ${Number(session)}, 'model_chunk_received', '\"idle\"', 0, json_object('n', x) FROM c;
      COMMIT;
    `);
    const scalePaths = [
      ['messages-newest', `/native/messages?session=${session}&limit=200`],
      ['messages-deep', `/native/messages?session=${session}&before=25001&limit=200`],
      ['search-hit', `/native/transcript/search?session=${session}&q=message%2049999&limit=50`],
      ['search-miss', `/native/transcript/search?session=${session}&q=absent-needle&limit=50`],
      ['events-head', `/native/events?session=${session}&after=0&limit=256`],
      ['events-deep', `/native/events?session=${session}&after=25000&limit=256`],
    ];
    const scale = {};
    if (scale_driver === null) {
      scale.skipped = 'no sqlite driver';
    } else {
      for (const [name, path] of scalePaths) {
        const samples = [];
        const responses = [];
        await Promise.all(
          Array.from({ length: 8 }, async () => {
            const started = performance.now();
            const result = await request(base, token, 'GET', path, undefined, MAX_BOUND_MS);
            samples.push(performance.now() - started);
            responses.push(result);
          }),
        );
        const sortedSamples = [...samples].sort((a, b) => a - b);
        scale[name] = {
          p95_ms: percentile(sortedSamples, 95),
          max_ms: sortedSamples[sortedSamples.length - 1],
          statuses: [...new Set(responses.map((r) => r.status))],
          max_bytes: Math.max(...responses.map((r) => Buffer.byteLength(r.text))),
        };
        if (responses.some((r) => r.status !== 200)) failures.push(`scale ${name} non-200`);
        const bound = name.startsWith('search') ? SEARCH_P95_BOUND_MS : P95_BOUND_MS;
        if (scale[name].p95_ms > bound) {
          failures.push(`scale ${name} p95 ${scale[name].p95_ms.toFixed(1)}ms > ${bound}ms`);
        }
        if (scale[name].max_bytes > 2 * 1024 * 1024) failures.push(`scale ${name} oversize`);
      }
    }
    const afterScaleRss = rss(daemon.child.pid);

    // Hostile burst: max-size cursor pages and unknown/hostile query values.
    const hostilePaths = [
      `/native/messages?session=${session}&limit=200`,
      `/native/events?session=${session}&after=0&limit=256`,
      `/native/messages?session=${session}&limit=201`,
      `/native/session/${session}/events?after=18446744073709551616`,
      '/native/outcomes?limit=200',
      '/native/sessions?limit=200',
    ];
    const burst = await Promise.all(
      Array.from({ length: 64 }, (_v, i) => {
        const path = hostilePaths[i % hostilePaths.length];
        return request(base, token, 'GET', path, undefined, MAX_BOUND_MS);
      }),
    );
    const burst5xx = burst.filter((r) => r.status !== null && r.status >= 500 && r.status !== 503);
    const burstOversize = burst.filter((r) => Buffer.byteLength(r.text) > 2 * 1024 * 1024);
    const afterBurstRss = rss(daemon.child.pid);

    const p95 = percentile(sorted, 95);
    if (latencies.length === 0) failures.push('no completed requests');
    if (p95 !== null && p95 > P95_BOUND_MS) failures.push(`p95 ${p95.toFixed(1)}ms > ${P95_BOUND_MS}ms`);
    if (sorted.length > 0 && sorted[sorted.length - 1] > MAX_BOUND_MS) failures.push(`max > ${MAX_BOUND_MS}ms`);
    if (burst5xx.length > 0) failures.push(`${burst5xx.length} burst 5xx responses`);
    if (burstOversize.length > 0) failures.push(`${burstOversize.length} burst responses over 2MiB`);
    const growth = beforeRss !== null && afterBurstRss !== null ? afterBurstRss - beforeRss : null;
    if (growth !== null && growth > RSS_GROWTH_BOUND_BYTES) failures.push(`RSS growth ${growth} bytes`);

    const document = {
      schema: 'faktor-independent-tail-probe/v1',
      generated_at: new Date().toISOString(),
      commit: execFileSync('git', ['-C', ROOT, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim(),
      concurrency: CONCURRENCY,
      duration_ms: DURATION_MS,
      requests: latencies.length,
      p50_ms: percentile(sorted, 50),
      p95_ms: p95,
      p99_ms: percentile(sorted, 99),
      max_ms: sorted.length > 0 ? sorted[sorted.length - 1] : null,
      statuses,
      burst: {
        requests: burst.length,
        five_xx: burst5xx.length,
        oversize: burstOversize.length,
        max_bytes: Math.max(0, ...burst.map((r) => Buffer.byteLength(r.text))),
      },
      rss_before_bytes: beforeRss,
      rss_after_steady_bytes: afterSteadyRss,
      rss_after_burst_bytes: afterBurstRss,
      rss_after_scale_bytes: afterScaleRss,
      scale_driver,
      rss_growth_bytes: growth,
      scale,
      digest: `sha256:${createHash('sha256').update(JSON.stringify(latencies)).digest('hex')}`,
      failures,
      passed: failures.length === 0,
    };
    if (report) {
      mkdirSync(dirname(OUT), { recursive: true });
      writeFileSync(OUT, `${JSON.stringify(document, null, 2)}\n`);
    }
    return { status: failures.length === 0 ? 'proven' : 'failed', document, failures };
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
  probeTail({ report: true })
    .then((result) => {
      console.log(JSON.stringify(result.document ?? result, null, 2));
      if (result.status === 'failed') process.exitCode = 1;
    })
    .catch((error) => {
      console.error(error.stack ?? error);
      process.exitCode = 1;
    });
}
