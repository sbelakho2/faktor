#!/usr/bin/env node
// TEST-SEPARATION: oracle=independent-reference isolation=separate-process
// Resource census wrapper (ledger HAR-11).
//
// Runs ANY suite command as a child process and samples its PROCESS TREE
// before, during and after the run:
//
//   - thread count          (peak over the run)
//   - open file handles     (peak over the run)
//   - child processes       (descendants that SURVIVE the root's exit —
//                            orphans; hard gate = 0)
//   - peak RSS of the tree  (and root RSS growth first-sample -> last-sample)
//   - sqlite connections    (handles whose path looks like a sqlite DB/WAL/
//                            SHM; reported per-run, informational)
//
// Emits one JSON report per run under target/resource-census/<label>.json
// (schema faktor-resource-census/v1) and exits 1 on any threshold breach.
//
// Orphan detection has TWO layers, because a reparented orphan is invisible
// in the ppid tree after the root dies:
//   1. during the run, every descendant pid is recorded (tight sampling);
//      after exit + grace, recorded pids that still exist are orphans;
//   2. the suite root is spawned in its OWN process group (setsid), so
//      after exit + grace `pgrep -g <pgid>` enumerates surviving group
//      members even when the root is gone (catches children the sampler
//      never saw because the root's lifetime was shorter than one tick).
// A grandchild that both outlives the suite AND calls setsid itself is
// caught only by layer 1; the default 100 ms sampling bounds that window.
//
// Usage:
//   node scripts/resource-census.mjs [options] -- <suite command...>
//   node scripts/resource-census.mjs --selftest
//
// Options:
//   --label NAME              report name (default: derived from command)
//   --out FILE                report path override
//   --max-orphans N           surviving descendants after exit (default 0)
//   --max-thread-peak N       peak threads in the tree (default 512)
//   --max-handle-peak N       peak open handles in the tree (default 4096)
//   --max-rss-mib N           peak RSS of the tree (default 6144)
//   --max-rss-growth-mib N    root RSS growth over the run (default 2048)
//   --sample-ms N             tree sampling period (default 100)
//   --grace-ms N              post-exit orphan grace period (default 1000)
//   --warn-only               report breaches but exit 0 (calibration)
//
// Exit codes: 0 all gates pass; 1 threshold breach; 2 usage/spawn failure.
//
// Per-platform capability matrix (graceful degradation, recorded in each
// report under `platform` + `gates.handlePeak.skipped`):
//
//   macOS (darwin)  — threads: ps -M -axo pid= (one row per thread, counted
//                     per pid, SUMMED OVER THE SUITE TREE ONLY)
//                     handles: lsof -p PID -F p for the <=8 largest tree
//                     pids (skipped with reason when lsof is absent)
//                     sqlite:  lsof -p PID -F n, path regex on
//                     *.db / *.sqlite(3) / *-wal / *-shm (same calls)
//                     children: ps -axo pid=,ppid= tree walk + pgrep -g
//                     rss:     ps -axo rss= (KiB), summed over the tree
//   Linux           — threads: /proc/<pid>/status `Threads:` (fallback
//                     ps -o nlwp=)
//                     handles: readdir /proc/<pid>/fd count (<=8 largest
//                     tree pids)
//                     sqlite:  readlink of /proc/<pid>/fd/* matched on the
//                     sqlite path regex (no lsof needed)
//                     children: ps -axo pid=,ppid= tree walk + pgrep -g
//                     rss:     ps -o rss= (KiB), summed over the tree
//   Windows         — not supported: exit 2 with this matrix (no portable
//                     thread/handle source is probed; census hosts are
//                     unix).
//
// Threshold defaults — chosen to catch REAL leaks without flaking:
//   orphans = 0        any descendant of a suite that outlives the suite is
//                      a genuine ownership leak (AGENTS commandment 8); the
//                      1s grace period absorbs ordinary shutdown reaping.
//   thread peak 512    the rustc/test harness is multi-threaded (per-core
//                      test threads + a handful of runtime pool threads);
//                      512 is ~8x the busiest legitimate local lane while a
//                      per-iteration thread leak in a 10k-iteration suite
//                      blows past it within seconds.
//   handle peak 4096   cargo+rustc hold hundreds of temp files, mapped
//                      objects and pipes at peak; a handle leak of even
//                      2/hundred iterations crosses 4096 in a long suite.
//   RSS peak 6144 MiB  the largest legit local compile+test peak observed
//                      is ~2 GiB; 6 GiB catches a multi-GB leak while
//                      leaving headroom for parallel sibling suites.
//   RSS growth 2048MiB first-sample -> last-sample growth of a STILL-ALIVE
//                      root; catches a ballooning long-running daemon-style
//                      suite while ignoring compile-settle transients.
// The selftest injects four deliberate leaks (orphaned child, leaked file
// handles, leaked worker threads, multi-hundred-MiB allocation) and
// requires every gate to fire; then a clean child must pass every gate.

import { spawn, spawnSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  readlinkSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const DEFAULTS = {
  maxOrphans: 0,
  maxThreadPeak: 512,
  maxHandlePeak: 4096,
  maxRssMib: 6144,
  maxRssGrowthMib: 2048,
  sampleMs: 100,
  graceMs: 1000,
};

const SQLITE_PATH = /(?:^|\/)[^/]*\.sqlite3?(?:-wal|-shm)?$/i;
const SQLITE_DB = /(?:^|\/)[^/]*\.db(?:-wal|-shm)?$/i;
const HANDLE_PID_CAP = 8;

function die(code, message) {
  console.error(`resource-census: ${message}`);
  process.exit(code);
}

function argValue(args, name, fallback) {
  const idx = args.indexOf(name);
  if (idx === -1) return fallback;
  const value = args[idx + 1];
  if (value === undefined || value.startsWith('--')) return fallback;
  return value;
}

function parseArgs(argv) {
  const settings = { ...DEFAULTS };
  for (const [flag, key] of [
    ['--max-orphans', 'maxOrphans'],
    ['--max-thread-peak', 'maxThreadPeak'],
    ['--max-handle-peak', 'maxHandlePeak'],
    ['--max-rss-mib', 'maxRssMib'],
    ['--max-rss-growth-mib', 'maxRssGrowthMib'],
    ['--sample-ms', 'sampleMs'],
    ['--grace-ms', 'graceMs'],
  ]) {
    const raw = argValue(argv, flag, null);
    if (raw !== null) {
      const n = Number(raw);
      if (!Number.isFinite(n) || n < 0) die(2, `${flag} needs a non-negative number, got ${raw}`);
      settings[key] = n;
    }
  }
  settings.label = argValue(argv, '--label', null);
  settings.out = argValue(argv, '--out', null);
  settings.warnOnly = argv.includes('--warn-only');
  settings.selftest = argv.includes('--selftest');
  const sep = argv.indexOf('--');
  settings.command = sep === -1 ? [] : argv.slice(sep + 1);
  return settings;
}

// ------------------------------------------------------------- capability

function have(cmd) {
  return spawnSync('which', [cmd], { stdio: 'ignore' }).status === 0;
}

function capabilities() {
  const os = process.platform;
  if (os === 'darwin') {
    return {
      os,
      arch: process.arch,
      threads: 'ps-M-axo-pid',
      handles: have('lsof') ? 'lsof-p' : 'unavailable',
      sqliteHandles: have('lsof') ? 'lsof-n-paths' : 'unavailable',
      children: 'ps-axo-ppid-tree+pgrep-g',
      rss: 'ps-axo-rss',
      procfs: false,
    };
  }
  if (os === 'linux') {
    const procfs = existsSync('/proc/self/status');
    return {
      os,
      arch: process.arch,
      threads: procfs ? 'proc-status-Threads' : 'ps-nlwp',
      handles: existsSync('/proc/self/fd') ? 'proc-fd-count' : (have('lsof') ? 'lsof-p' : 'unavailable'),
      sqliteHandles: existsSync('/proc/self/fd')
        ? 'proc-fd-readlink'
        : (have('lsof') ? 'lsof-n-paths' : 'unavailable'),
      children: 'ps-axo-ppid-tree+pgrep-g',
      rss: 'ps-axo-rss',
      procfs,
    };
  }
  return {
    os,
    arch: process.arch,
    threads: 'unavailable',
    handles: 'unavailable',
    sqliteHandles: 'unavailable',
    children: 'unavailable',
    rss: 'unavailable',
    procfs: false,
  };
}

// ------------------------------------------------------------- sampling

/** One full process table: pid -> { ppid, rssKiB } (first 3 ps fields). */
function processTable() {
  const probe = spawnSync('ps', ['-axo', 'pid=,ppid=,rss='], {
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  });
  const table = new Map();
  if (probe.status !== 0) return table;
  for (const line of probe.stdout.split('\n')) {
    const parts = line.trim().split(/\s+/);
    if (parts.length < 3) continue;
    const pid = Number(parts[0]);
    const ppid = Number(parts[1]);
    const rssKiB = Number(parts[2]);
    if (!Number.isFinite(pid) || !Number.isFinite(ppid)) continue;
    table.set(pid, { ppid, rssKiB: Number.isFinite(rssKiB) ? rssKiB : 0 });
  }
  return table;
}

/** Thread count per pid, ONLY for the given suite-tree pids. */
function threadCounts(caps, pids) {
  const wanted = new Set(pids);
  const out = new Map();
  if (caps.threads === 'ps-M-axo-pid') {
    const probe = spawnSync('ps', ['-M', '-axo', 'pid='], {
      encoding: 'utf8',
      maxBuffer: 64 * 1024 * 1024,
    });
    if (probe.status !== 0) return out;
    for (const line of probe.stdout.split('\n')) {
      const fields = line.trim().split(/\s+/);
      const pid = Number(fields[fields.length - 1]);
      if (!Number.isFinite(pid) || !wanted.has(pid)) continue;
      out.set(pid, (out.get(pid) ?? 0) + 1);
    }
    return out;
  }
  if (caps.procfs) {
    for (const pid of pids) {
      try {
        const status = readFileSync(`/proc/${pid}/status`, 'utf8');
        const m = /^Threads:\s*(\d+)\s*$/m.exec(status);
        if (m) out.set(pid, Number(m[1]));
      } catch {
        // process exited between table and status read
      }
    }
    return out;
  }
  if (caps.threads === 'ps-nlwp') {
    for (const pid of pids) {
      const probe = spawnSync('ps', ['-o', 'nlwp=', '-p', String(pid)], { encoding: 'utf8' });
      const n = Number((probe.stdout ?? '').trim());
      if (probe.status === 0 && Number.isFinite(n)) out.set(pid, n);
    }
  }
  return out;
}

/** Handle + sqlite counts for at most HANDLE_PID_CAP largest tree pids. */
function handleCounts(caps, table, pids) {
  const out = new Map();
  const sqlite = new Map();
  if (caps.handles === 'unavailable') return { handles: out, sqlite };
  const ranked = [...pids]
    .sort((a, b) => (table.get(b)?.rssKiB ?? 0) - (table.get(a)?.rssKiB ?? 0))
    .slice(0, HANDLE_PID_CAP);
  const useProc = existsSync('/proc/self/fd');
  for (const pid of ranked) {
    if (useProc) {
      try {
        const fds = readdirSync(`/proc/${pid}/fd`);
        out.set(pid, fds.length);
        let sq = 0;
        for (const fd of fds) {
          try {
            const target = readlinkSync(`/proc/${pid}/fd/${fd}`);
            if (SQLITE_PATH.test(target) || SQLITE_DB.test(target)) sq += 1;
          } catch {
            // fd closed mid-scan
          }
        }
        sqlite.set(pid, sq);
      } catch {
        // process gone
      }
      continue;
    }
    const probe = spawnSync('lsof', ['-p', String(pid), '-F', 'fn'], {
      encoding: 'utf8',
      maxBuffer: 32 * 1024 * 1024,
    });
    const lines = (probe.stdout ?? '').split('\n');
    let count = 0;
    let sq = 0;
    for (const line of lines) {
      if (line.startsWith('f')) count += 1;
      if (line.startsWith('n') && (SQLITE_PATH.test(line) || SQLITE_DB.test(line))) sq += 1;
    }
    if (count > 0) out.set(pid, count);
    if (sq > 0) sqlite.set(pid, sq);
  }
  return { handles: out, sqlite };
}

function descendants(rootPid, table) {
  const children = new Map();
  for (const [pid, info] of table) {
    if (!children.has(info.ppid)) children.set(info.ppid, []);
    children.get(info.ppid).push(pid);
  }
  const tree = [];
  const queue = [rootPid];
  while (queue.length > 0) {
    const pid = queue.shift();
    for (const child of children.get(pid) ?? []) {
      tree.push(child);
      queue.push(child);
    }
  }
  return tree;
}

/** Surviving members of the suite's process group (layer-2 orphans). */
function groupSurvivors(pgid) {
  if (!have('pgrep')) return [];
  const probe = spawnSync('pgrep', ['-g', String(pgid)], { encoding: 'utf8' });
  if (probe.status !== 0) return [];
  return probe.stdout
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => Number(line))
    .filter((pid) => Number.isFinite(pid) && pid > 0 && pid !== pgid);
}

function sum(values) {
  let total = 0;
  for (const value of values) total += value;
  return total;
}

// ------------------------------------------------------------- the run

async function runCensus(settings) {
  const caps = capabilities();
  if (caps.os !== 'darwin' && caps.os !== 'linux') {
    die(
      2,
      `unsupported platform '${caps.os}' — capability matrix: threads/handles/children/rss ` +
        'have no portable source probed on this OS (supported: darwin, linux)',
    );
  }
  if (settings.command.length === 0) die(2, 'no suite command given (usage: ... -- <command...>)');

  const label =
    settings.label ??
    settings.command
      .map((part) => part.replace(/[^A-Za-z0-9._-]+/g, '-'))
      .join('-')
      .slice(0, 80);

  const startedAt = new Date().toISOString();
  const t0 = Date.now();

  // detached => the suite is its own session/process-group leader, so its
  // orphans stay enumerable (pgrep -g) after the root itself is gone.
  const child = spawn(settings.command[0], settings.command.slice(1), {
    stdio: 'inherit',
    env: process.env,
    detached: true,
  });
  const rootPid = child.pid;
  if (!rootPid) die(2, `failed to spawn: ${settings.command.join(' ')}`);

  const peaks = { threads: 0, handles: 0, rssKiB: 0, treePids: 0, sqliteHandles: 0 };
  let before = null;
  let lastRootRss = null;
  let firstRootRss = null;
  const seenDescendants = new Map(); // pid -> { pid, ppid }
  let handleCapability = caps.handles;
  let emptyHandleSamples = 0;
  let ticks = 0;
  let exited = false;

  const sampleOnce = (includeHandles) => {
    const table = processTable();
    const tree = descendants(rootPid, table);
    const live = [rootPid, ...tree].filter((pid) => table.has(pid));
    const threads = threadCounts(caps, live);
    let handles = new Map();
    let sqlite = new Map();
    if (includeHandles && handleCapability !== 'unavailable') {
      const result = handleCounts(caps, table, live);
      handles = result.handles;
      sqlite = result.sqlite;
      if (handles.size === 0 && live.length > 0) {
        emptyHandleSamples += 1;
        if (emptyHandleSamples >= 3) handleCapability = 'unavailable';
      } else {
        emptyHandleSamples = 0;
      }
    }
    for (const pid of tree) {
      if (!seenDescendants.has(pid)) {
        seenDescendants.set(pid, { pid, ppid: table.get(pid)?.ppid ?? null });
      }
    }
    peaks.threads = Math.max(peaks.threads, sum(threads.values()));
    peaks.handles = Math.max(peaks.handles, sum(handles.values()));
    peaks.rssKiB = Math.max(peaks.rssKiB, sum(live.map((pid) => table.get(pid)?.rssKiB ?? 0)));
    peaks.treePids = Math.max(peaks.treePids, live.length);
    peaks.sqliteHandles = Math.max(peaks.sqliteHandles, sum(sqlite.values()));
    const rootRss = table.get(rootPid)?.rssKiB ?? null;
    if (rootRss !== null) {
      if (firstRootRss === null) firstRootRss = rootRss;
      lastRootRss = rootRss;
    }
    if (before === null) {
      before = {
        rootPid,
        treePids: live.length,
        threads: sum(threads.values()),
        handles: handleCapability === 'unavailable' ? null : sum(handles.values()),
        rssKiB: sum(live.map((pid) => table.get(pid)?.rssKiB ?? 0)),
        sqliteHandles: handleCapability === 'unavailable' ? null : sum(sqlite.values()),
      };
    }
    ticks += 1;
  };

  const exitPromise = new Promise((resolveExit, rejectExit) => {
    child.on('error', (error) => rejectExit(error));
    child.on('exit', (code, signal) => {
      exited = true;
      resolveExit({ code, signal });
    });
  });

  sampleOnce(true);
  const sampler = setInterval(() => {
    if (!exited) sampleOnce(ticks % 3 === 0);
  }, Math.max(20, settings.sampleMs));

  let status;
  try {
    status = await exitPromise;
  } catch (error) {
    clearInterval(sampler);
    die(2, `spawn failed: ${error.message}`);
  }
  clearInterval(sampler);

  // Grace period: ordinary child reaping settles here. Afterwards a pid is
  // an orphan when (a) the sampler saw it as a descendant and it still
  // exists, or (b) it still sits in the suite's process group.
  await new Promise((r) => setTimeout(r, settings.graceMs));
  const finalTable = processTable();
  const recorded = [...seenDescendants.values()].filter(
    (entry) => entry.pid !== rootPid && entry.pid > 2,
  );
  const orphans = [];
  const orphanPids = new Set();
  for (const entry of recorded) {
    if (finalTable.has(entry.pid)) {
      orphanPids.add(entry.pid);
      orphans.push({
        pid: entry.pid,
        layer: 'recorded-descendant',
        ppid: finalTable.get(entry.pid)?.ppid ?? null,
        rssKiB: finalTable.get(entry.pid)?.rssKiB ?? null,
      });
    }
  }
  for (const pid of groupSurvivors(rootPid)) {
    if (!orphanPids.has(pid)) {
      orphanPids.add(pid);
      orphans.push({
        pid,
        layer: 'process-group',
        ppid: finalTable.get(pid)?.ppid ?? null,
        rssKiB: finalTable.get(pid)?.rssKiB ?? null,
      });
    }
  }

  const durationMs = Date.now() - t0;
  const rssGrowthMiB =
    firstRootRss !== null && lastRootRss !== null
      ? Math.max(0, Math.round((lastRootRss - firstRootRss) / 1024))
      : 0;

  const gates = {
    orphans: {
      limit: settings.maxOrphans,
      observed: orphans.length,
      ok: orphans.length <= settings.maxOrphans,
    },
    threadPeak: {
      limit: settings.maxThreadPeak,
      observed: peaks.threads,
      ok: peaks.threads <= settings.maxThreadPeak,
    },
    handlePeak: {
      limit: settings.maxHandlePeak,
      observed: handleCapability === 'unavailable' ? null : peaks.handles,
      skipped: handleCapability === 'unavailable',
      reason: handleCapability === 'unavailable' ? 'no lsof/procfs handle source on this host' : null,
      ok: handleCapability === 'unavailable' || peaks.handles <= settings.maxHandlePeak,
    },
    rssPeakMiB: {
      limit: settings.maxRssMib,
      observed: Math.round(peaks.rssKiB / 1024),
      ok: Math.round(peaks.rssKiB / 1024) <= settings.maxRssMib,
    },
    rssGrowthMiB: {
      limit: settings.maxRssGrowthMib,
      observed: rssGrowthMiB,
      ok: rssGrowthMiB <= settings.maxRssGrowthMib,
    },
  };
  const breaches = Object.entries(gates)
    .filter(([, gate]) => !gate.ok)
    .map(([name]) => name);

  const report = {
    schema: 'faktor-resource-census/v1',
    label,
    command: settings.command,
    platform: { ...caps, handleSource: handleCapability },
    startedAt,
    durationMs,
    exitCode: status.code,
    signal: status.signal,
    samples: ticks,
    before,
    peaks: {
      threads: peaks.threads,
      handles: handleCapability === 'unavailable' ? null : peaks.handles,
      rssKiB: peaks.rssKiB,
      treePids: peaks.treePids,
      sqliteHandles: handleCapability === 'unavailable' ? null : peaks.sqliteHandles,
    },
    after: {
      rootAlive: finalTable.has(rootPid),
      survivingDescendants: orphans,
      descendantPidsSeen: seenDescendants.size,
    },
    gates,
    ok: breaches.length === 0 && status.code === 0,
    breaches: [...breaches, ...(status.code === 0 ? [] : [`suite_exit_code_${status.code}`])],
  };

  const outPath = settings.out ?? join(ROOT, 'target', 'resource-census', `${label}.json`);
  mkdirSync(dirname(outPath), { recursive: true });
  writeFileSync(outPath, `${JSON.stringify(report, null, 2)}\n`);
  console.error(
    `resource-census: ${label} exit=${status.code} orphans=${orphans.length} ` +
      `threads_peak=${peaks.threads} handles_peak=${report.peaks.handles ?? 'n/a'} ` +
      `rss_peak_mib=${Math.round(peaks.rssKiB / 1024)} report=${outPath}`,
  );
  if (report.ok) return { report, outPath, code: 0 };
  return { report, outPath, code: settings.warnOnly ? 0 : 1 };
}

// ------------------------------------------------------------- selftest

function writeLeakFixtures(dir) {
  const files = {
    'clean.mjs': 'process.exit(0);\n',
    // The orphan is a plain child (no setsid): it stays in the suite's
    // process group, so the post-exit group probe must find it even though
    // the suite's lifetime is shorter than one sampling tick.
    'orphan.mjs':
      "import { spawn } from 'node:child_process';\n" +
      "spawn('sleep', ['30'], { stdio: 'ignore' });\n" +
      'setTimeout(() => process.exit(0), 50);\n',
    'handles.mjs':
      "import { openSync } from 'node:fs';\n" +
      'const held = [];\n' +
      'for (let i = 0; i < 3000; i += 1) held.push(openSync("/dev/null", "r"));\n' +
      // Keep the leak observable across several sampling ticks.
      'setTimeout(() => process.exit(0), 900);\n',
    'threads.mjs':
      "import { Worker } from 'node:worker_threads';\n" +
      'const workers = [];\n' +
      "for (let i = 0; i < 8; i += 1) {\n" +
      "  workers.push(new Worker('Atomics.wait(new Int32Array(new SharedArrayBuffer(4)),0,0);', { eval: true }));\n" +
      '}\n' +
      'setTimeout(() => process.exit(0), 900);\n',
    'rss.mjs':
      'const held = Buffer.alloc(300 * 1024 * 1024, 7);\n' +
      'if (held[0] !== 7) process.exit(9);\n' +
      'setTimeout(() => process.exit(0), 900);\n',
  };
  mkdirSync(dir, { recursive: true });
  for (const [name, body] of Object.entries(files)) writeFileSync(join(dir, name), body);
  return dir;
}

async function selftest() {
  const dir = writeLeakFixtures(join(tmpdir(), `faktor-census-selftest-${process.pid}`));
  const scenarios = [
    { name: 'clean', file: 'clean.mjs', expectCode: 0, expectBreach: null, settings: {} },
    { name: 'leak-orphan', file: 'orphan.mjs', expectCode: 1, expectBreach: 'orphans', settings: {} },
    {
      name: 'leak-handles',
      file: 'handles.mjs',
      expectCode: 1,
      expectBreach: 'handlePeak',
      settings: { '--max-handle-peak': '2000' },
    },
    {
      name: 'leak-threads',
      file: 'threads.mjs',
      expectCode: 1,
      expectBreach: 'threadPeak',
      settings: { '--max-thread-peak': '4' },
    },
    {
      name: 'leak-rss',
      file: 'rss.mjs',
      expectCode: 1,
      expectBreach: 'rssPeakMiB',
      settings: { '--max-rss-mib': '200' },
    },
  ];
  let failures = 0;
  for (const scenario of scenarios) {
    const settings = parseArgs([
      '--label',
      `selftest-${scenario.name}`,
      '--grace-ms',
      '700',
      ...Object.entries(scenario.settings).flat(),
      '--',
      'node',
      join(dir, scenario.file),
    ]);
    const { report, code } = await runCensus(settings);
    const breachFired =
      scenario.expectBreach === null
        ? report.breaches.filter((b) => b !== `suite_exit_code_${report.exitCode}`).length === 0
        : report.breaches.includes(scenario.expectBreach);
    const codeOk = code === scenario.expectCode;
    if (breachFired && codeOk) {
      console.log(
        `selftest ok: '${scenario.name}' ${
          scenario.expectBreach === null ? 'passes clean' : `fires ${scenario.expectBreach}`
        }`,
      );
    } else {
      console.error(
        `selftest FAIL: '${scenario.name}' expected breach=${scenario.expectBreach} code=${scenario.expectCode}, ` +
          `got breaches=${JSON.stringify(report.breaches)} code=${code}`,
      );
      failures += 1;
    }
    // Kill EXACTLY the pids the report recorded as surviving (the injected
    // orphan) — never a pattern match that could hit a sibling's process,
    // and NEVER pid 0/1/2 (kill(0) would signal our own process group).
    for (const orphan of report.after.survivingDescendants) {
      if (!Number.isInteger(orphan.pid) || orphan.pid <= 2) continue;
      try {
        process.kill(orphan.pid, 'SIGKILL');
      } catch {
        // already gone
      }
    }
  }
  try {
    rmSync(dir, { recursive: true, force: true });
  } catch {
    // best-effort fixture cleanup
  }
  if (failures > 0) {
    console.error(`resource-census selftest: FAIL (${failures} scenario(s))`);
    return 1;
  }
  console.log('resource-census selftest: PASS (all four injected leaks detected, clean child passes)');
  return 0;
}

// ---------------------------------------------------------------- main

async function main() {
  const argv = process.argv.slice(2);
  if (argv.includes('-h') || argv.includes('--help')) {
    console.log(
      readFileSync(fileURLToPath(import.meta.url), 'utf8')
        .split('\n')
        .slice(1, 63)
        .join('\n'),
    );
    process.exit(0);
  }
  const settings = parseArgs(argv);
  if (settings.selftest) process.exit(await selftest());
  const { code } = await runCensus(settings);
  process.exit(code);
}

main().catch((error) => die(2, error.stack ?? String(error)));
