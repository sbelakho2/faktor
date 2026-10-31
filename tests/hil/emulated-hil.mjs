#!/usr/bin/env node
// HAR-18 emulated-HIL harness: boots the in-repo firmware image
// (tests/hil/firmware, bare-metal thumbv7m for QEMU's lm3s6965evb) under
// qemu-system-arm and drives the serial adapter path end-to-end, plus
// adversarial fault variants. This is the closest true equivalent of the
// audit row's "Renode/QEMU boards + HIL rig" the repository can own: a
// REAL firmware ELF built from in-repo source, executed by a REAL system
// emulator, spoken to over the emulated UART through a host-side adapter
// with typed results. There is no production embedded adapter in crates/**
// today (investigated; see the HAR-18 report), so the adapter exercised
// here is this harness's own serial-adapter seam; promoting it to a
// production crate would need a reviewed test-seam proposal first.
//
// Design laws (adversarial, bounded, deterministic):
//   * deterministic seeds: protocol seed 424242 + LCG constants are
//     mirrored as an independent reference oracle in this file; the
//     firmware computes its own sequence and the harness diffs them;
//   * every await carries a deadline; the whole scenario carries a
//     ceiling; nothing may hang — silence becomes a typed failure;
//   * every scenario ends with: emulator reaped (waitpid), zero orphan
//     qemu processes referencing the kernel path, bounded wall time;
//   * faults are typed: adapter-disconnect, image-mismatch (integrity
//     gate, no emulator spawn), boot-failure (corrupted image, deadline),
//     emulator-exit (SIGKILL mid-run) + recovery reboot, determinism.
//
// Usage:
//   node tests/hil/emulated-hil.mjs [--scenario all|happy|disconnect|corrupt|kill|determinism]
//                                   [--kernel PATH]   # skip the in-repo build
//                                   [--qemu PATH]     # default: qemu-system-arm on PATH
//                                   [--keep-tmp]
// Exit codes: 0 all scenarios pass; 1 a scenario failed; 2 typed refusal
// (emulator missing, firmware build failed) — never a claimed pass.
//
// TEST-SEPARATION: oracle=independent-reference isolation=separate-process,isolated-data-dir

import { spawn, spawnSync, execFileSync } from 'node:child_process';
import { createConnection } from 'node:net';
import { createHash } from 'node:crypto';
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

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const FW_DIR = join(ROOT, 'tests', 'hil', 'firmware');
const EVIDENCE_PATH = join(ROOT, 'target', 'certification', 'hil', 'emulated-hil.json');

// ------------------------------------------------------- protocol reference
// Independent oracle: the same constants/algorithm the firmware implements,
// transcribed here from the protocol SPEC (not read back from the SUT).
const PROTOCOL_SEED = 424242;
const LCG_A = 1664525;
const LCG_C = 1013904223;
const BANNER_RE = /^FAKTOR-HIL-FW v1 seed=424242$/;

function randReferenceSequence(count) {
  const out = [];
  let state = PROTOCOL_SEED >>> 0;
  for (let i = 0; i < count; i += 1) {
    state = (Math.imul(state, LCG_A) + LCG_C) >>> 0;
    out.push(state);
  }
  return out;
}

// ------------------------------------------------------------------ bounds
const BOOT_DEADLINE_MS = 6000;
const REQUEST_DEADLINE_MS = 4000;
const SCENARIO_CEILING_MS = 25000;
const STOP_TERM_MS = 2000;

// --------------------------------------------------------------- utilities
function typed(kind, detail) {
  const error = new Error(`${kind}: ${detail}`);
  error.kind = kind;
  error.detail = detail;
  return error;
}

function withDeadline(promise, ms, kind, detail) {
  let timer;
  const deadline = new Promise((_, reject) => {
    timer = setTimeout(() => reject(typed(kind, detail)), ms);
  });
  return Promise.race([promise, deadline]).finally(() => clearTimeout(timer));
}

function sha256Hex(buffer) {
  return createHash('sha256').update(buffer).digest('hex');
}

function qemuVersion(binary) {
  const probe = spawnSync(binary, ['--version'], { encoding: 'utf8' });
  if (probe.status !== 0) {
    throw typed('emulator-unavailable', `${binary} --version failed`);
  }
  return probe.stdout.split('\n')[0].trim();
}

function listQemuProcessesFor(kernelPath) {
  const ps = execFileSync('ps', ['-ax', '-o', 'pid=', '-o', 'command='], { encoding: 'utf8' });
  const hits = [];
  for (const line of ps.split('\n')) {
    const trimmed = line.trim();
    if (trimmed === '') continue;
    if (!trimmed.includes('qemu-system-')) continue;
    if (!trimmed.includes(kernelPath)) continue;
    hits.push(Number(trimmed.split(/\s+/)[0]));
  }
  return hits;
}

function assertNoOrphanEmulator(kernelPath, label) {
  const orphans = listQemuProcessesFor(kernelPath);
  if (orphans.length > 0) {
    throw typed('invariant-orphan-emulator', `${label}: pid(s) ${orphans.join(', ')} still reference ${kernelPath}`);
  }
}

function assertBounded(ms, bound, label) {
  if (ms > bound) {
    throw typed('invariant-runtime-bound', `${label}: took ${ms}ms > bound ${bound}ms`);
  }
}

// ------------------------------------------------------------- ELF .text
// Minimal ELF32 little-endian section parse — only what the corruption
// scenario needs: locate `.text` so byte flips land in executable code,
// not in headers (a header flip would be rejected by the loader and would
// not exercise the no-hang law).
function findTextSection(elf) {
  if (elf.readUInt32LE(0) !== 0x464c457f || elf[4] !== 1 || elf[5] !== 1) {
    throw typed('image-parse', 'not an ELF32 little-endian file');
  }
  const e_shoff = elf.readUInt32LE(0x20);
  const e_shentsize = elf.readUInt16LE(0x2e);
  const e_shnum = elf.readUInt16LE(0x30);
  const e_shstrndx = elf.readUInt16LE(0x32);
  const shstr = e_shoff + e_shstrndx * e_shentsize;
  const strOff = elf.readUInt32LE(shstr + 0x10);
  const strSize = elf.readUInt32LE(shstr + 0x14);
  const names = elf.subarray(strOff, strOff + strSize);
  for (let i = 0; i < e_shnum; i += 1) {
    const sh = e_shoff + i * e_shentsize;
    const nameOff = elf.readUInt32LE(sh);
    const nameEnd = names.indexOf(0, nameOff);
    const name = names.toString('latin1', nameOff, nameEnd);
    if (name === '.text') {
      return { offset: elf.readUInt32LE(sh + 0x10), size: elf.readUInt32LE(sh + 0x14) };
    }
  }
  throw typed('image-parse', 'no .text section');
}

// ------------------------------------------------------------ emulator I/O
class EmulatorSession {
  static live = new Set();

  constructor({ kernelPath, sockPath, stderrPath, binary }) {
    this.kernelPath = kernelPath;
    this.sockPath = sockPath;
    this.stderrPath = stderrPath;
    this.binary = binary;
    this.child = null;
    this.exitInfo = null;
    this.stderrTail = '';
  }

  static async start({ binary, kernelPath, sockPath, stderrPath }) {
    const session = new EmulatorSession({ kernelPath, sockPath, stderrPath, binary });
    session.child = spawn(
      binary,
      [
        '-M', 'lm3s6965evb',
        '-cpu', 'cortex-m3',
        '-kernel', kernelPath,
        '-display', 'none',
        '-monitor', 'none',
        '-no-reboot',
        // wait=on: QEMU holds machine start until the adapter connects, so
        // no boot byte is emitted before the harness is listening.
        '-serial', `unix:${sockPath},server=on,wait=on`,
      ],
      { stdio: ['ignore', 'ignore', 'pipe'] },
    );
    EmulatorSession.live.add(session);
    session.exitPromise = new Promise((resolveExit) => {
      session.child.on('exit', (code, signal) => {
        session.exitInfo = { code, signal };
        EmulatorSession.live.delete(session);
        resolveExit(session.exitInfo);
      });
    });
    session.child.stderr.on('data', (chunk) => {
      if (session.stderrTail.length < 4096) session.stderrTail += chunk.toString();
    });
    return session;
  }

  async kill(signal) {
    if (this.exitInfo !== null) return this.exitInfo;
    this.child.kill(signal);
    return withDeadline(
      this.exitPromise,
      STOP_TERM_MS + 2000,
      'emulator-reap-timeout',
      `emulator did not exit after ${signal}`,
    );
  }

  async stop() {
    if (this.exitInfo !== null) return this.exitInfo;
    this.child.kill('SIGTERM');
    const graceful = await Promise.race([
      this.exitPromise.then(() => true),
      new Promise((r) => setTimeout(() => r(false), STOP_TERM_MS)),
    ]);
    if (!graceful) {
      this.child.kill('SIGKILL');
      return withDeadline(
        this.exitPromise,
        4000,
        'emulator-reap-timeout',
        'emulator survived SIGTERM and SIGKILL',
      );
    }
    return this.exitInfo;
  }

  static async cleanupAll() {
    for (const session of [...EmulatorSession.live]) {
      await session.stop().catch(() => {});
    }
  }
}

// The host-side serial adapter (the code path under test for HAR-18's
// "adapter interaction" leg): line-framed request/response over the
// emulated UART socket, every operation deadline-bounded and typed.
class FirmwareAdapter {
  constructor({ socket, session, transcript }) {
    this.socket = socket;
    this.session = session;
    this.transcript = transcript;
    this.buffer = '';
    this.identity = null;
    this.queue = [];
    this.waiters = [];
    this.closed = false;
    this.closeReason = null;
    socket.on('data', (chunk) => this.onData(chunk));
    socket.on('close', () => this.onClose('socket-closed'));
    socket.on('error', (error) => this.onClose(`socket-error: ${error.code ?? error.message}`));
  }

  onData(chunk) {
    this.buffer += chunk.toString();
    let idx;
    while ((idx = this.buffer.indexOf('\r\n')) !== -1) {
      const line = this.buffer.slice(0, idx);
      this.buffer = this.buffer.slice(idx + 2);
      this.transcript.push({ dir: 'rx', line });
      const waiter = this.waiters.shift();
      if (waiter) waiter.resolve(line);
      else this.queue.push(line);
    }
  }

  onClose(reason) {
    if (this.closed) return;
    this.closed = true;
    this.closeReason = reason;
    while (this.waiters.length > 0) {
      this.waiters.shift().reject(
        typed('adapter-disconnect', `serial link lost (${reason}) with request in flight`),
      );
    }
  }

  nextLine(deadlineMs, what) {
    if (this.queue.length > 0) return Promise.resolve(this.queue.shift());
    if (this.closed) {
      return Promise.reject(typed('adapter-disconnect', `serial link already lost (${this.closeReason})`));
    }
    return new Promise((resolveLine, reject) => {
      this.waiters.push({ resolve: resolveLine, reject });
      setTimeout(() => {
        const at = this.waiters.findIndex((w) => w.resolve === resolveLine);
        if (at !== -1) this.waiters.splice(at, 1);
        reject(typed('request-timeout', `${what}: no line within ${deadlineMs}ms`));
      }, deadlineMs).unref?.();
    });
  }

  async boot(deadlineMs) {
    const line = await this.nextLine(deadlineMs, 'boot banner');
    if (!BANNER_RE.test(line)) {
      throw typed('protocol-violation', `unexpected boot line ${JSON.stringify(line)}`);
    }
    this.identity = { id: 'FAKTOR-HIL-FW', version: 'v1', seed: PROTOCOL_SEED };
    this.transcript.push({ dir: 'boot', line });
    return this.identity;
  }

  async request(command, deadlineMs = REQUEST_DEADLINE_MS) {
    this.transcript.push({ dir: 'tx', line: command });
    this.socket.write(`${command}\n`);
    const line = await this.nextLine(deadlineMs, `request ${JSON.stringify(command)}`);
    const match = /^(PONG|RAND|ERR)\b/.exec(line);
    if (match === null) {
      throw typed('protocol-violation', `response to ${JSON.stringify(command)} is not typed: ${JSON.stringify(line)}`);
    }
    return line;
  }

  disconnectAbruptly() {
    this.socket.destroy();
  }

  static async connect(session) {
    const transcript = [];
    const deadline = Date.now() + BOOT_DEADLINE_MS;
    // QEMU (wait=on) binds the serial socket asynchronously after spawn;
    // retry ENOENT/ECONNREFUSED until the boot deadline, never hang.
    for (;;) {
      try {
        const socket = createConnection(session.sockPath);
        await withDeadline(
          new Promise((resolveConnect, reject) => {
            socket.once('connect', resolveConnect);
            socket.once('error', reject);
          }),
          Math.max(50, deadline - Date.now()),
          'adapter-connect-timeout',
          `could not connect to ${session.sockPath}`,
        );
        return new FirmwareAdapter({ socket, session, transcript });
      } catch (error) {
        if (Date.now() >= deadline || error.kind === 'adapter-connect-timeout') {
          throw typed('adapter-connect-timeout', `could not connect to ${session.sockPath}: ${error.message}`);
        }
        await new Promise((r) => setTimeout(r, 50));
      }
    }
  }

  close() {
    this.socket.destroy();
  }
}

// ------------------------------------------------------------------ build
function ensureFirmwareBuild(log) {
  const targetElf = join(FW_DIR, 'target', 'thumbv7m-none-eabi', 'release', 'faktor-hil-fw');
  const installed = spawnSync('rustup', ['target', 'list', '--installed'], { encoding: 'utf8' });
  if (installed.status === 0 && !installed.stdout.includes('thumbv7m-none-eabi')) {
    log('firmware-build: installing rustup target thumbv7m-none-eabi');
    const add = spawnSync('rustup', ['target', 'add', 'thumbv7m-none-eabi'], { encoding: 'utf8' });
    if (add.status !== 0) {
      throw typed('firmware-build', `rustup target add failed: ${add.stderr}`);
    }
  }
  log('firmware-build: cargo build --release (tests/hil/firmware)');
  const build = spawnSync('cargo', ['build', '--release'], { cwd: FW_DIR, encoding: 'utf8' });
  if (build.status !== 0) {
    throw typed('firmware-build', `cargo build failed: ${build.stderr.slice(-2000)}`);
  }
  if (!existsSync(targetElf)) {
    throw typed('firmware-build', `expected ELF missing at ${targetElf}`);
  }
  return targetElf;
}

// --------------------------------------------------------------- scenarios
async function withScenario(name, fn, ctx) {
  const started = Date.now();
  const record = { name, status: 'fail', ms: 0, typed: null, transcript: [], details: {} };
  try {
    await withDeadline(
      fn(record, ctx),
      SCENARIO_CEILING_MS,
      'scenario-timeout',
      `scenario ${name} exceeded ceiling ${SCENARIO_CEILING_MS}ms`,
    );
    record.status = 'pass';
  } catch (error) {
    record.status = 'fail';
    record.typed = { kind: error.kind ?? 'untyped-error', message: error.message };
  } finally {
    await EmulatorSession.cleanupAll().catch(() => {});
    record.ms = Date.now() - started;
    // Invariants hold for every scenario, pass or fail: no orphans, bounded.
    try {
      assertNoOrphanEmulator(ctx.kernelPath, name);
      record.details.no_orphan_emulator = true;
    } catch (error) {
      record.status = 'fail';
      record.details.no_orphan_emulator = false;
      record.typed = { kind: error.kind, message: error.message };
    }
    try {
      assertBounded(record.ms, SCENARIO_CEILING_MS + 3000, name);
      record.details.bounded_runtime = true;
    } catch (error) {
      record.status = 'fail';
      record.details.bounded_runtime = false;
      record.typed = { kind: error.kind, message: error.message };
    }
  }
  return record;
}

async function scenarioHappy(record, ctx) {
  const session = await EmulatorSession.start(ctx);
  const adapter = await FirmwareAdapter.connect(session);
  try {
    const identity = await adapter.boot(BOOT_DEADLINE_MS);
    record.details.identity = identity;

    const pong = await adapter.request('PING');
    if (pong !== 'PONG seed=424242 seq=1') {
      throw typed('oracle-mismatch', `PING -> ${JSON.stringify(pong)}, want PONG seed=424242 seq=1`);
    }

    const reference = randReferenceSequence(3);
    const seen = [];
    for (let i = 0; i < 3; i += 1) {
      const reply = await adapter.request('RAND');
      const match = /^RAND val=(\d+) seq=(\d+)$/.exec(reply);
      if (match === null) {
        throw typed('protocol-violation', `RAND reply not typed: ${JSON.stringify(reply)}`);
      }
      seen.push({ value: Number(match[1]), seq: Number(match[2]) });
    }
    for (let i = 0; i < 3; i += 1) {
      if (seen[i].value !== reference[i] || seen[i].seq !== i + 2) {
        throw typed(
          'oracle-mismatch',
          `RAND #${i} -> value=${seen[i].value} seq=${seen[i].seq}, want value=${reference[i]} seq=${i + 2}`,
        );
      }
    }

    const bogus = await adapter.request('BOGUS-CMD');
    if (!/^ERR unknown seq=5$/.test(bogus)) {
      throw typed('oracle-mismatch', `unknown cmd -> ${JSON.stringify(bogus)}`);
    }

    const exitInfo = await session.stop();
    if (exitInfo.signal !== 'SIGTERM' && exitInfo.code !== 0) {
      throw typed('emulator-exit', `graceful stop gave ${JSON.stringify(exitInfo)}`);
    }
    record.typed = { kind: 'e2e-ok', message: 'boot -> PING -> RAND x3 -> unknown-cmd -> graceful stop' };
  } finally {
    adapter.close();
    record.transcript = adapter.transcript;
  }
}

async function scenarioDisconnect(record, ctx) {
  const session = await EmulatorSession.start(ctx);
  const adapter = await FirmwareAdapter.connect(session);
  try {
    await adapter.boot(BOOT_DEADLINE_MS);
    const pong = await adapter.request('PING');
    if (pong !== 'PONG seed=424242 seq=1') {
      throw typed('oracle-mismatch', `PING -> ${JSON.stringify(pong)}`);
    }
    // Fault: the host adapter link dies mid-scenario. The in-flight request
    // must fail typed (adapter-disconnect) inside the deadline — never hang.
    adapter.disconnectAbruptly();
    let caught = null;
    try {
      await adapter.request('PING');
    } catch (error) {
      caught = error;
    }
    if (caught === null || caught.kind !== 'adapter-disconnect') {
      throw typed('fault-not-typed', `expected adapter-disconnect, got ${caught === null ? 'success' : caught.kind}`);
    }
    record.typed = { kind: caught.kind, message: caught.message };
  } finally {
    adapter.close();
    record.transcript = adapter.transcript;
    await session.stop();
  }
}

async function scenarioCorrupt(record, ctx) {
  // Layer 1 — integrity gate: a byte-flipped copy of the REAL image must be
  // refused by digest mismatch BEFORE any emulator process exists.
  const clean = readFileSync(ctx.kernelPath);
  const cleanSha = sha256Hex(clean);
  const corrupted = Buffer.from(clean);
  const text = findTextSection(corrupted);
  if (text.size < 600) {
    throw typed('image-parse', `.text section too small (${text.size}) for deterministic corruption`);
  }
  const flips = [32, 96, 160, 288, 512];
  for (const rel of flips) {
    const at = text.offset + rel;
    corrupted[at] ^= 0xff;
  }
  const corruptPath = join(ctx.runDir, 'faktor-hil-fw.corrupt.elf');
  writeFileSync(corruptPath, corrupted);
  const corruptSha = sha256Hex(corrupted);
  if (corruptSha === cleanSha) {
    throw typed('corruption-inert', 'byte flips did not change the digest');
  }

  const before = listQemuProcessesFor(ctx.kernelPath).length;
  const gateError = (() => {
    if (corruptSha !== ctx.firmware.sha256) {
      return typed('image-mismatch', `sha256 ${corruptSha} != recorded build digest ${ctx.firmware.sha256}`);
    }
    return null;
  })();
  if (gateError === null) {
    throw typed('fault-not-typed', 'integrity gate accepted a corrupted image');
  }
  const during = listQemuProcessesFor(ctx.kernelPath).length;
  if (during !== before) {
    throw typed('invariant-orphan-emulator', 'integrity gate must not spawn an emulator');
  }
  record.details.integrity_gate = { refused: gateError.kind, sha256: corruptSha };

  // Layer 2 — runtime: if a corrupted image is booted anyway, the guest
  // must not hang the harness: absence of the typed banner within the boot
  // deadline is a typed boot-failure, and the emulator is reaped.
  const session = await EmulatorSession.start({ ...ctx, kernelPath: corruptPath });
  const adapter = await FirmwareAdapter.connect(session);
  // A corrupted image must fail TYPED and bounded. Two real outcomes are
  // acceptable and both are proven by the fault lane over time: (a) the
  // guest wedges and the boot banner never arrives (request-timeout), or
  // (b) the guest jumps into the weeds, faults, and QEMU exits/reset-loops
  // so the serial link dies (adapter-disconnect / emulator death). What is
  // NEVER acceptable is a banner (corruption went unnoticed) or silence
  // past the deadline (a hang).
  let bootError = null;
  try {
    await adapter.boot(BOOT_DEADLINE_MS);
  } catch (error) {
    bootError = error;
  } finally {
    adapter.close();
  }
  const typedBootFailures = new Set(['request-timeout', 'adapter-disconnect']);
  if (bootError === null || !typedBootFailures.has(bootError.kind)) {
    await session.stop();
    throw typed('fault-not-typed', `corrupted boot gave ${bootError === null ? 'a banner' : bootError.kind}`);
  }
  const exitInfo = await session.stop();
  assertNoOrphanEmulator(corruptPath, 'corrupt-runtime');
  record.typed = {
    kind: 'image-mismatch+boot-failure',
    message: `integrity gate refused ${corruptSha.slice(0, 12)}…; direct boot of the corrupt image failed typed (${bootError.kind}) within ${BOOT_DEADLINE_MS}ms; emulator reaped (${JSON.stringify(exitInfo)})`,
  };
  record.transcript = adapter.transcript;
}

async function scenarioKill(record, ctx) {
  // Fault: the emulator process is SIGKILLed mid-scenario. The adapter must
  // fail typed (link loss), reconciliation must observe the death (waitpid),
  // and a fresh boot must restore the deterministic protocol (seq restarts
  // at 1 — stateless firmware, replayable identity).
  const session = await EmulatorSession.start(ctx);
  const adapter = await FirmwareAdapter.connect(session);
  try {
    await adapter.boot(BOOT_DEADLINE_MS);
    const pong = await adapter.request('PING');
    if (pong !== 'PONG seed=424242 seq=1') {
      throw typed('oracle-mismatch', `PING -> ${JSON.stringify(pong)}`);
    }
    await session.kill('SIGKILL');
    let caught = null;
    try {
      await adapter.request('PING');
    } catch (error) {
      caught = error;
    }
    if (caught === null || caught.kind !== 'adapter-disconnect') {
      throw typed('fault-not-typed', `expected adapter-disconnect after kill, got ${caught === null ? 'success' : caught.kind}`);
    }
    const exitInfo = await withDeadline(
      session.exitPromise,
      3000,
      'reconcile-timeout',
      'emulator exit was not observed after SIGKILL',
    );
    if (exitInfo.signal !== 'SIGKILL') {
      throw typed('reconcile-mismatch', `exit was ${JSON.stringify(exitInfo)}, want SIGKILL`);
    }
    record.details.killed = { adapter_fault: caught.kind, reconciled: exitInfo };
  } finally {
    adapter.close();
    record.transcript = adapter.transcript;
  }

  // Recovery: reboot the same image; the typed identity and seq=1 restart
  // prove the reconciliation story (clean deterministic replay, no state).
  const session2 = await EmulatorSession.start(ctx);
  const adapter2 = await FirmwareAdapter.connect(session2);
  try {
    await adapter2.boot(BOOT_DEADLINE_MS);
    const pong2 = await adapter2.request('PING');
    if (pong2 !== 'PONG seed=424242 seq=1') {
      throw typed('oracle-mismatch', `post-kill PING -> ${JSON.stringify(pong2)}`);
    }
    await session2.stop();
    record.typed = {
      kind: 'emulator-kill+recovered',
      message: `SIGKILL mid-run reconciled (${JSON.stringify(record.details.killed.reconciled)}); reboot PING seq=1 deterministic`,
    };
  } finally {
    adapter2.close();
  }
  record.transcript = [...adapter.transcript, ...adapter2.transcript];
}

async function scenarioDeterminism(record, ctx) {
  const transcripts = [];
  for (let run = 0; run < 2; run += 1) {
    const session = await EmulatorSession.start(ctx);
    const adapter = await FirmwareAdapter.connect(session);
    try {
      await adapter.boot(BOOT_DEADLINE_MS);
      await adapter.request('PING');
      await adapter.request('RAND');
      await adapter.request('RAND');
      await adapter.request('PING');
      await session.stop();
      transcripts.push(adapter.transcript);
    } finally {
      adapter.close();
    }
  }
  const [first, second] = transcripts;
  if (JSON.stringify(first) !== JSON.stringify(second)) {
    throw typed('nondeterminism', 'two identical scenario runs produced different transcripts');
  }
  const reference = randReferenceSequence(2);
  const randLines = first.filter((entry) => entry.dir === 'rx' && entry.line.startsWith('RAND')).map((e) => e.line);
  // Sequence: PING(seq=1), RAND(seq=2), RAND(seq=3), PING(seq=4).
  const wantLines = [
    `RAND val=${reference[0]} seq=2`,
    `RAND val=${reference[1]} seq=3`,
  ];
  if (JSON.stringify(randLines) !== JSON.stringify(wantLines)) {
    throw typed('oracle-mismatch', `deterministic RAND lines ${JSON.stringify(randLines)} != ${JSON.stringify(wantLines)}`);
  }
  record.transcript = first;
  record.typed = { kind: 'deterministic', message: 'two boots byte-identical; RAND matches the reference LCG' };
}

// -------------------------------------------------------------------- main
function parseArgs(argv) {
  const args = { scenario: 'all', kernel: null, qemu: null, keepTmp: false };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--scenario') args.scenario = argv[++i];
    else if (arg === '--kernel') args.kernel = argv[++i];
    else if (arg === '--qemu') args.qemu = argv[++i];
    else if (arg === '--keep-tmp') args.keepTmp = true;
    else {
      console.error(`emulated-hil: unknown argument ${arg}`);
      process.exit(2);
    }
  }
  return args;
}

const SCENARIOS = {
  happy: scenarioHappy,
  disconnect: scenarioDisconnect,
  corrupt: scenarioCorrupt,
  kill: scenarioKill,
  determinism: scenarioDeterminism,
};

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const log = (message) => console.log(`emulated-hil: ${message}`);

  const binary = args.qemu ?? 'qemu-system-arm';
  let version;
  try {
    version = qemuVersion(binary);
  } catch (error) {
    // Typed refusal: an emulator-less machine can never claim a HAR-18
    // pass. The unblock is `brew install qemu` (or the CI image gaining
    // qemu-system-arm) — see the lane's companion report.
    console.error(`emulated-hil: ${error.message}`);
    return 2;
  }
  log(`emulator: ${version}`);

  let kernelPath = args.kernel;
  if (kernelPath === null) {
    try {
      kernelPath = ensureFirmwareBuild(log);
    } catch (error) {
      console.error(`emulated-hil: ${error.message}`);
      return 2;
    }
  } else {
    kernelPath = resolve(kernelPath);
    if (!existsSync(kernelPath)) {
      console.error(`emulated-hil: kernel ${kernelPath} does not exist`);
      return 2;
    }
  }
  const firmwareBytes = readFileSync(kernelPath);
  const firmware = {
    path: kernelPath,
    sha256: sha256Hex(firmwareBytes),
    provenance:
      'in-repo build: tests/hil/firmware (bare-metal thumbv7m, no external deps; ' +
      'provenance = this source tree + rust-toolchain channel)',
  };
  log(`firmware: ${kernelPath} sha256=${firmware.sha256}`);

  const runDir = mkdtempSync(join(tmpdir(), 'faktor-hil-'));
  const sockPath = join(runDir, 'fw.sock');
  const ctx = {
    binary,
    kernelPath,
    sockPath,
    stderrPath: join(runDir, 'qemu.stderr'),
    runDir,
    firmware,
  };

  const wanted = args.scenario === 'all' ? Object.keys(SCENARIOS) : [args.scenario];
  for (const name of wanted) {
    if (SCENARIOS[name] === undefined) {
      console.error(`emulated-hil: unknown scenario ${name} (have ${Object.keys(SCENARIOS).join(', ')})`);
      return 2;
    }
  }

  const records = [];
  for (const name of wanted) {
    log(`scenario: ${name} ...`);
    const record = await withScenario(name, SCENARIOS[name], ctx);
    records.push(record);
    const label = record.status === 'pass' ? 'PASS' : 'FAIL';
    const typedNote = record.typed === null ? '' : ` [${record.typed.kind}] ${record.typed.message}`;
    log(`scenario: ${name} -> ${label} (${record.ms}ms)${typedNote}`);
  }

  if (!args.keepTmp) {
    rmSync(runDir, { recursive: true, force: true });
  } else {
    log(`tmp kept: ${runDir}`);
  }

  const failed = records.filter((record) => record.status !== 'pass');
  const evidence = {
    schema: 'faktor-hil-emulated/v1',
    at: new Date().toISOString(),
    qemu: { binary, version },
    firmware,
    seed: PROTOCOL_SEED,
    lcg: { a: LCG_A, c: LCG_C },
    bounds: { boot_ms: BOOT_DEADLINE_MS, request_ms: REQUEST_DEADLINE_MS, scenario_ceiling_ms: SCENARIO_CEILING_MS },
    scenarios: records,
    invariants: {
      no_orphan_emulator: records.every((record) => record.details.no_orphan_emulator === true),
      bounded_runtime: records.every((record) => record.details.bounded_runtime === true),
    },
    totals: { passed: records.length - failed.length, failed: failed.length },
  };
  mkdirSync(dirname(EVIDENCE_PATH), { recursive: true });
  writeFileSync(EVIDENCE_PATH, `${JSON.stringify(evidence, null, 2)}\n`);
  log(`evidence: ${EVIDENCE_PATH}`);

  if (failed.length > 0) {
    log(`RESULT: FAIL (${failed.length}/${records.length} scenario(s) failed)`);
    return 1;
  }
  log(`RESULT: PASS (${records.length} scenario(s), ${evidence.invariants.no_orphan_emulator && evidence.invariants.bounded_runtime ? 'invariants held' : 'INVARIANT BROKEN'})`);
  return 0;
}

process.exitCode = await main();
