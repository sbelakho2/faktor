#!/usr/bin/env node
// Shared lane-marker generator: the ONE place that turns a lane's commands,
// commit/tree, runner and artifacts into a `faktor-woodpecker-lane/v2` marker
// and (when the lane's own CI secret env is present) into a per-lane
// HMAC-SHA256 binding.
//
// Why this exists: marker fields were previously assembled inline in every
// `.woodpecker/**` bash/PowerShell step, so every lane could fabricate any
// other lane's marker from values derivable from the YAML and CI_* env.
// The marker now carries
//
//   "auth": "hmac-sha256:<hex>"
//
// where the MAC is HMAC-SHA256(key = the lane's own `faktor_lane_token_<lane>`
// secret env, message = the canonical JSON of the marker WITHOUT `auth`).
// Because each lane step only receives its own token, a marker renamed to
// another lane (or with any tampered field) no longer verifies; the
// certificate step, which receives every lane token, rejects it.
//
// When the lane has no token env (PR/untrusted, unconfigured operator), the
// marker is emitted WITHOUT `auth` and `verify-markers` records the lane as
// `unattested` instead of `passed` — visible, never a silent pass. That keeps
// PR/untrusted behavior working with no hard failure while binding is not
// configured.
//
// This file remains the readable reference implementation. The lane-facing
// entry point is `scripts/certification/lane-marker.sh`, which runs the Rust
// `target/release/faktor-ci-tools lane-marker` when built and otherwise this
// file with node. `evidence.mjs` proves both produce byte-identical marker
// JSON and the same canonical signing form.
//
// Usage (normally via lane-marker.sh):
//   printf '%s' "$CMDS" | node scripts/certification/lane-marker.mjs write \
//     --lane linux --status passed \
//     [--reason TEXT] [--artifact PATH]... [--optional-artifact PATH]... \
//     [--flake-file target/certification/flakes/linux.json] \
//     [--commands TEXT | --commands-file FILE] \
//     [--out target/certification/lanes/linux.json] [--cwd DIR] \
//     [--commit SHA] [--tree SHA] [--run-id ID] [--started-at ISO] \
//     [--finished-at ISO] [--runner-os OS] [--runner-arch ARCH] [--runner-ci CI]
//
// `--flake-file` reads the `faktor-flake-tolerance/v1` report the
// flake-tolerance runner wrote for THIS lane, refuses an inconsistent report
// (foreign lane, failed/persistent status, count mismatches), attaches the
// report file as a marker artifact (so its digest is bound) and records the
// bounded `flakes` summary inside the marker: the flake list is visible in
// the certification artifact and covered by the marker HMAC.

import { createHash, createHmac } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const MARKER_SCHEMA = 'faktor-woodpecker-lane/v2';

export function sha256Hex(text) {
  return createHash('sha256').update(text, 'utf8').digest('hex');
}

// Canonical JSON: object keys sorted recursively, no whitespace. This is the
// exact signing form; the verifier in evidence.mjs implements the same rules.
export function canonicalJson(value) {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalJson).join(',')}]`;
  }
  if (value && typeof value === 'object') {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(',')}}`;
  }
  return JSON.stringify(value === undefined ? null : value);
}

export function artifactLines(artifacts) {
  return artifacts
    .slice()
    .sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0))
    .map((a) => `${a.sha256}\t${a.path}\n`)
    .join('');
}

export function artifactDigest(artifacts) {
  return `sha256:${sha256Hex(artifactLines(artifacts))}`;
}

export function hashFile(path) {
  const data = readFileSync(path);
  return `sha256:${createHash('sha256').update(data).digest('hex')}`;
}

// The marker payload that is signed: everything except `auth`. Any other
// field (lane, status, commit, tree, runner, timestamps, commands, artifacts)
// is covered by the MAC.
export function markerAuthPayload(record) {
  const { auth, ...rest } = record;
  void auth;
  return canonicalJson(rest);
}

export function computeMarkerAuth(record, token) {
  const mac = createHmac('sha256', String(token)).update(markerAuthPayload(record), 'utf8').digest('hex');
  return `hmac-sha256:${mac}`;
}

// Secret env naming: the lane id is lowercased and every run of non-alnum
// characters becomes `_`, e.g. `jetbrains-smoke` -> `faktor_lane_token_jetbrains_smoke`.
// The uppercase form is accepted as a convenience for host/step environments
// that canonicalize names.
export function normalizeLane(lane) {
  return String(lane)
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '_')
    .replace(/^_+|_+$/g, '');
}

export function laneTokenEnvNames(lane) {
  const normalized = normalizeLane(lane);
  return [`faktor_lane_token_${normalized}`, `FAKTOR_LANE_TOKEN_${normalized.toUpperCase()}`];
}

export function laneTokenFromEnv(lane, env = process.env) {
  for (const name of laneTokenEnvNames(lane)) {
    const value = env[name];
    if (typeof value === 'string' && value.length > 0) {
      return value;
    }
  }
  return '';
}

export function isoNow() {
  return new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
}

function isoFromEpoch(raw) {
  if (!/^\d+$/.test(String(raw || ''))) {
    return null;
  }
  const date = new Date(Number(raw) * 1000);
  return Number.isFinite(date.getTime()) ? date.toISOString().replace(/\.\d{3}Z$/, 'Z') : null;
}

function defaultStartedAt() {
  const raw = process.env.CI_PIPELINE_STARTED || '';
  const asEpoch = isoFromEpoch(raw);
  if (asEpoch) {
    return asEpoch;
  }
  if (raw) {
    const parsed = new Date(raw);
    if (Number.isFinite(parsed.getTime())) {
      return parsed.toISOString().replace(/\.\d{3}Z$/, 'Z');
    }
  }
  return isoNow();
}

function defaultRunnerOs() {
  return process.platform === 'darwin'
    ? 'darwin'
    : process.platform === 'win32'
      ? 'windows'
      : process.platform === 'linux'
        ? 'linux'
        : 'unknown';
}

function defaultRunnerArch() {
  const mapped = { x64: 'x86_64', arm64: 'arm64', ia32: 'i686' }[process.arch];
  return mapped || process.arch || 'unknown';
}

function git(args, cwd) {
  return execFileSync('git', ['-C', cwd, ...args], { encoding: 'utf8' }).trim();
}

/**
 * TRUE when the tracked working tree is clean: `git diff --exit-code` and
 * `git diff --cached --exit-code` both succeed. Any git failure (not a
 * worktree, missing git) is recorded as NOT clean, so the verifier fails
 * closed rather than certifying bytes nobody proved.
 */
function workingTreeClean(cwd) {
  for (const args of [
    ['diff', '--exit-code', '--quiet'],
    ['diff', '--cached', '--exit-code', '--quiet'],
  ]) {
    try {
      execFileSync('git', ['-C', cwd, ...args], { stdio: 'ignore' });
    } catch {
      return false;
    }
  }
  return true;
}

function flagValue(args, name, fallback = '') {
  const idx = args.indexOf(name);
  if (idx === -1) {
    return fallback;
  }
  const value = args[idx + 1];
  if (value === undefined || value.startsWith('--')) {
    return '';
  }
  return value;
}

function flagValues(args, name) {
  const values = [];
  for (let i = 0; i < args.length; i += 1) {
    if (args[i] === name && i + 1 < args.length && !args[i + 1].startsWith('--')) {
      values.push(args[i + 1]);
    }
  }
  return values;
}

// Read the lane's CMDS heredoc from stdin. `readFileSync(0)` is not used:
// when the writer leaves the pipe momentarily empty (non-blocking pipe), it
// throws EAGAIN instead of waiting, which would silently produce an empty
// command set. Retry EAGAIN until EOF.
function readStdin() {
  if (process.stdin.isTTY) {
    return '';
  }
  const chunks = [];
  const buffer = Buffer.alloc(65536);
  for (;;) {
    let bytes;
    try {
      bytes = readSync(0, buffer, 0, buffer.length, null);
    } catch (error) {
      if (error.code === 'EAGAIN' || error.code === 'EINTR') {
        Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 5);
        continue;
      }
      throw error;
    }
    if (bytes === 0) {
      break;
    }
    chunks.push(Buffer.from(buffer.subarray(0, bytes)));
  }
  return Buffer.concat(chunks).toString('utf8');
}

function usage() {
  console.error(`usage: lane-marker.mjs write --lane ID [--status passed|failed|skipped] \\
  [--reason TEXT] [--artifact PATH]... [--optional-artifact PATH]... \\
  [--flake-file target/certification/flakes/ID.json] \\
  [--commands TEXT | --commands-file FILE] [--out FILE] [--cwd DIR] \\
  [--commit SHA] [--tree SHA] [--run-id ID] [--started-at ISO] [--finished-at ISO] \\
  [--runner-os OS] [--runner-arch ARCH] [--runner-ci CI]

Commands default to stdin (pipe the lane's CMDS heredoc into write).
Token: lane's own CI secret env (faktor_lane_token_<lane>, uppercase accepted).`);
}

/**
 * Read + validate the flake-tolerance report for THIS lane and turn it into
 * the bounded marker summary. A report that cannot certify the lane (foreign
 * lane, persistent failures, budget breach, count mismatch) is refused loud:
 * tolerance is only ever recorded for the exact lane that earned it.
 */
export function flakeSummary(report, lane, file) {
  const fail = (message) => {
    throw new Error(`--flake-file ${file}: ${message}`);
  };
  if (!report || typeof report !== 'object' || Array.isArray(report)) {
    fail('not a JSON object');
  }
  if (report.schema !== 'faktor-flake-tolerance/v1') {
    fail(`schema '${report.schema}' is not faktor-flake-tolerance/v1`);
  }
  if (report.lane !== lane) {
    fail(`report lane '${report.lane}' is not '${lane}'`);
  }
  const status = String(report.status);
  if (!['clean', 'passed_with_flakes'].includes(status)) {
    fail(`status '${status}' cannot certify a passed lane`);
  }
  const flakes = Array.isArray(report.flakes) ? report.flakes : fail('flakes is not an array');
  const tests = flakes.map((flake) => {
    if (!flake || typeof flake.test !== 'string' || flake.test.length === 0) {
      fail('flake entry without a test name');
    }
    if (flake.isolated_result !== 'passed' || flake.isolation_attempts !== 1) {
      fail(`flake '${flake.test}' is not a once-retried isolated pass`);
    }
    return flake.test;
  });
  if (status === 'clean' && tests.length !== 0) {
    fail('clean report carries flakes');
  }
  if (status === 'passed_with_flakes' && tests.length === 0) {
    fail('passed_with_flakes report carries no flakes');
  }
  if (Array.isArray(report.persistent_failures) && report.persistent_failures.length > 0) {
    fail('report carries persistent failures and cannot certify a passed lane');
  }
  return { schema: report.schema, status, count: tests.length, tests, report: file };
}

export function generateMarker(options) {
  const {
    lane,
    status = 'passed',
    reasonProvided = false,
    reason = '',
    commandsText = '',
    artifacts = [],
    commit,
    tree,
    startedAt,
    finishedAt,
    runner,
    token = '',
    flakes = null,
  } = options;
  if (!lane || typeof lane !== 'string') {
    throw new Error('--lane is required');
  }
  if (!['passed', 'failed', 'skipped'].includes(status)) {
    throw new Error(`--status '${status}' is not passed|failed|skipped`);
  }
  if (typeof commandsText !== 'string' || commandsText.length === 0) {
    throw new Error('no commands text: pipe the lane commands on stdin or pass --commands/--commands-file');
  }
  const record = {
    schema: MARKER_SCHEMA,
    lane,
    status,
    ...(reasonProvided ? { reason } : {}),
    // Recorded flake tolerance (bounded summary of the runner's report);
    // absent unless the lane passed `--flake-file`.
    ...(flakes ? { flakes } : {}),
    commit,
    tree,
    runner,
    started_at: startedAt,
    finished_at: finishedAt,
    commands_b64: Buffer.from(commandsText, 'utf8').toString('base64'),
    commands_digest: `sha256:${sha256Hex(commandsText)}`,
    artifacts,
    artifact_digest: artifactDigest(artifacts),
    // P0-CERT: certifying lanes must have executed the COMMITTED bytes. The
    // verifier rejects a lane whose marker records a dirty working tree
    // (tracked modifications or staged changes), because HEAD^{tree} is not
    // what ran. Untracked build outputs do not count as dirty.
    clean: options.clean === undefined ? true : options.clean === true,
  };
  if (token) {
    record.auth = computeMarkerAuth(record, token);
  }
  return record;
}

function writeCommand(args) {
  const cwd = resolve(flagValue(args, '--cwd', '.'));
  const lane = flagValue(args, '--lane');
  if (!lane) {
    usage();
    return 2;
  }
  let commandsText = '';
  const commandsFile = flagValue(args, '--commands-file');
  if (commandsFile) {
    commandsText = readFileSync(resolve(cwd, commandsFile), 'utf8');
  } else if (args.includes('--commands')) {
    commandsText = flagValue(args, '--commands');
  } else {
    commandsText = readStdin();
  }
  const artifacts = [];
  for (const path of flagValues(args, '--artifact')) {
    const full = resolve(cwd, path);
    if (!existsSync(full) || !statSync(full).isFile()) {
      throw new Error(`--artifact ${path} is not a file in ${cwd}`);
    }
    artifacts.push({ path, sha256: hashFile(full) });
  }
  for (const path of flagValues(args, '--optional-artifact')) {
    const full = resolve(cwd, path);
    if (existsSync(full) && statSync(full).isFile()) {
      artifacts.push({ path, sha256: hashFile(full) });
    }
  }
  let flakes = null;
  const flakeFile = flagValue(args, '--flake-file');
  if (flakeFile) {
    const full = resolve(cwd, flakeFile);
    let parsed;
    try {
      parsed = JSON.parse(readFileSync(full, 'utf8'));
    } catch (error) {
      throw new Error(`--flake-file ${flakeFile}: ${error.message}`);
    }
    flakes = flakeSummary(parsed, lane, flakeFile);
    if (!artifacts.some((artifact) => artifact.path === flakeFile)) {
      artifacts.push({ path: flakeFile, sha256: hashFile(full) });
    }
  }
  let commit = flagValue(args, '--commit') || process.env.CI_COMMIT_SHA || '';
  if (!commit) {
    commit = git(['rev-parse', 'HEAD'], cwd);
  }
  let tree = flagValue(args, '--tree') || '';
  if (!tree) {
    try {
      tree = git(['rev-parse', 'HEAD^{tree}'], cwd);
    } catch {
      tree = 'unknown';
    }
  }
  const record = generateMarker({
    lane,
    status: flagValue(args, '--status', 'passed'),
    reasonProvided: args.includes('--reason'),
    reason: flagValue(args, '--reason'),
    commandsText,
    artifacts,
    commit,
    tree,
    startedAt: flagValue(args, '--started-at') || defaultStartedAt(),
    finishedAt: flagValue(args, '--finished-at') || isoNow(),
    runner: {
      os: flagValue(args, '--runner-os') || defaultRunnerOs(),
      arch: flagValue(args, '--runner-arch') || defaultRunnerArch(),
      ci: flagValue(args, '--runner-ci') || (process.env.CI ? 'woodpecker' : 'local'),
      run_id: flagValue(args, '--run-id') || process.env.CI_PIPELINE_NUMBER || '0',
    },
    clean: workingTreeClean(cwd),
    token: laneTokenFromEnv(lane),
    flakes,
  });
  const out = resolve(cwd, flagValue(args, '--out', `target/certification/lanes/${lane}.json`));
  mkdirSync(dirname(out), { recursive: true });
  writeFileSync(out, `${JSON.stringify(record)}\n`);
  console.log(`lane-marker: ${out} lane=${lane} status=${record.status} auth=${record.auth ? 'hmac-sha256' : 'none'}`);
  return 0;
}

function main(argv) {
  const [command, ...args] = argv;
  if (!command || ['-h', '--help', 'help'].includes(command)) {
    usage();
    return command ? 0 : 2;
  }
  try {
    if (command === 'write') {
      return writeCommand(args);
    }
    console.error(`lane-marker: unknown command '${command}'`);
    usage();
    return 2;
  } catch (error) {
    console.error(`lane-marker: ${error.message}`);
    return 1;
  }
}

const invokedDirectly = (() => {
  if (!process.argv[1]) {
    return false;
  }
  try {
    return realpathSync(process.argv[1]) === realpathSync(fileURLToPath(import.meta.url));
  } catch {
    return false;
  }
})();

if (invokedDirectly) {
  process.exit(main(process.argv.slice(2)));
}
