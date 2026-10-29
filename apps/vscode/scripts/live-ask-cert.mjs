// TEST-SEPARATION: oracle=external-fixture isolation=separate-process
//! Real-provider certification for the conversational pipeline (the audit's
//! "fluidity + image" acceptance): spawns a THROWAWAY daemon (no user data),
//! then proves against the REAL provider that
//!   1. an Ask attachment actually reaches the model (a solid-magenta PNG is
//!      attached and the model must name that colour — information it can
//!      only have from the image), and
//!   2. the live-output contract carries first tokens (frames arrive before
//!      the durable turn completes, byte offsets are contiguous), Stop keeps
//!      the partial reply, and the durable transcript reconciles byte-exactly
//!      with the streamed text (no duplication, no loss).
//!
//! Usage: node apps/vscode/scripts/live-ask-cert.mjs [--image] [--fluidity] [--all]
//! Requires a provider env file (default ~/.faktor-live/deepseek.env) and the
//! faktor binary (default ~/.faktor-live/bin/faktor).

import { spawn } from 'node:child_process';
import { createHash, randomUUID } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { request } from 'node:http';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';

const HOME = process.env.HOME ?? '';
const BIN = process.env.FAKTOR_CERT_BIN ?? join(HOME, '.faktor-live', 'bin', 'faktor');
const ENV_FILE = process.env.FAKTOR_CERT_ENV ?? join(HOME, '.faktor-live', 'deepseek.env');
const args = new Set(process.argv.slice(2));
const runImage = args.has('--all') || args.has('--image');
const runFluidity = args.has('--all') || args.has('--fluidity');

let CURRENT_DAEMON = null;
function fail(message) {
  if (CURRENT_DAEMON !== null && CURRENT_DAEMON.stderrTail().length > 0) {
    console.error(`daemon stderr tail: ${CURRENT_DAEMON.stderrTail().slice(-2500)}`);
  }
  console.error(`CERT FAIL: ${message}`);
  process.exit(1);
}

function requireFile(path, what) {
  if (!existsSync(path)) fail(`${what} missing: ${path}`);
  return path;
}

function providerEnv() {
  const env = {};
  for (const line of readFileSync(requireFile(ENV_FILE, 'provider env'), 'utf8').split('\n')) {
    const m = /^([A-Z0-9_]+)=(.*)$/.exec(line.trim());
    if (m) env[m[1]] = m[2];
  }
  return env;
}

/** A solid-magenta 8x8 PNG, hand-assembled (no image libraries). */
function magentaPng() {
  const zlib = (data) => {
    // store-only deflate stream (valid, uncompressed)
    let out = Buffer.from([0x78, 0x01]);
    let offset = 0;
    while (offset < data.length) {
      const chunk = data.subarray(offset, Math.min(offset + 65535, data.length));
      const final = offset + chunk.length >= data.length ? 1 : 0;
      const header = Buffer.alloc(5);
      header[0] = final;
      header.writeUInt16LE(chunk.length, 1);
      header.writeUInt16LE(~chunk.length & 0xffff, 3);
      const csum = (() => {
        let a = 1;
        let b = 0;
        for (const byte of chunk) {
          a = (a + byte) % 65521;
          b = (b + a) % 65521;
        }
        return (b << 16) | a;
      })();
      const trailer = Buffer.alloc(4);
      trailer.writeUInt32BE(csum >>> 0, 0);
      out = Buffer.concat([out, header, chunk, trailer]);
      offset += chunk.length;
    }
    return Buffer.concat([out, Buffer.from([0, 0, 0, 0])]);
  };
  const width = 8;
  const height = 8;
  const stride = width * 4 + 1;
  const raw = Buffer.alloc(stride * height);
  for (let y = 0; y < height; y += 1) {
    raw[y * stride] = 0;
    for (let x = 0; x < width; x += 1) {
      const at = y * stride + 1 + x * 4;
      raw[at] = 0xff; // R
      raw[at + 1] = 0x00; // G
      raw[at + 2] = 0xff; // B
      raw[at + 3] = 0xff; // A
    }
  }
  const chunk = (type, data) => {
    const head = Buffer.alloc(8);
    head.writeUInt32BE(data.length, 0);
    head.write(type, 4, 'ascii');
    const crc = (() => {
      let c = 0xffffffff;
      const table = [];
      for (let n = 0; n < 256; n += 1) {
        let v = n;
        for (let k = 0; k < 8; k += 1) v = v & 1 ? 0xedb88320 ^ (v >>> 1) : v >>> 1;
        table[n] = v >>> 0;
      }
      for (const byte of Buffer.concat([Buffer.from(type, 'ascii'), data])) {
        c = table[(c ^ byte) & 0xff] ^ (c >>> 8);
      }
      return (c ^ 0xffffffff) >>> 0;
    })();
    const tail = Buffer.alloc(4);
    tail.writeUInt32BE(crc, 0);
    return Buffer.concat([head, data, tail]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8;
  ihdr[9] = 6;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib(raw)),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

function httpJson({ port, method, path, token, body }) {
  return new Promise((resolve, reject) => {
    const payload = body === undefined ? null : Buffer.from(JSON.stringify(body));
    const req = request(
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
      (res) => {
        const chunks = [];
        res.on('data', (c) => chunks.push(c));
        res.on('end', () => {
          const text = Buffer.concat(chunks).toString('utf8');
          let json = null;
          try {
            json = JSON.parse(text);
          } catch {
            /* non-json */
          }
          resolve({ status: res.statusCode, json, text });
        });
      },
    );
    req.on('error', reject);
    if (payload !== null) req.write(payload);
    req.end();
  });
}

/** SSE live-output subscription; resolves with collected frames. */
function subscribeLive({ port, token, sessionId }) {
  const frames = [];
  let buffer = '';
  const started = Date.now();
  const req = request(
    {
      host: '127.0.0.1',
      port,
      method: 'GET',
      path: `/native/session/${sessionId}/live/output`,
      headers: { authorization: `Bearer ${token}`, accept: 'text/event-stream' },
    },
    (res) => {
      if (res.statusCode !== 200) {
        frames.push({ at: Date.now() - started, event: 'status', data: { status: res.statusCode } });
        res.resume();
        return;
      }
      res.setEncoding('utf8');
      res.on('data', (chunk) => {
        buffer += chunk;
        for (;;) {
          const split = buffer.indexOf('\n\n');
          if (split < 0) break;
          const block = buffer.slice(0, split);
          buffer = buffer.slice(split + 2);
          let event = 'message';
          for (const line of block.split('\n')) {
            if (line.startsWith('event: ')) event = line.slice(7).trim();
            if (line.startsWith('data: ')) {
              let data = null;
              try {
                data = JSON.parse(line.slice(6));
              } catch {
                /* ignore */
              }
              frames.push({ at: Date.now() - started, event, data });
            }
          }
        }
      });
    },
  );
  req.on('error', () => {
    /* connection dropped is part of the contract under test */
  });
  req.end();
  return { frames, close: () => req.destroy() };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function startDaemon(env) {
  const dir = mkdtempSync(join(tmpdir(), 'faktor-cert-'));
  const dataDir = join(dir, 'data');
  const password = randomUUID().replace(/-/g, '') + randomUUID().replace(/-/g, '');
  mkdirSync(dataDir, { recursive: true });
  const liveConfig = join(HOME, '.faktor-live', 'data', 'faktor-plus.json');
  if (existsSync(liveConfig)) {
    // Provider registration rides the operator's own config file (no secrets
    // are read here; the provider env carries credentials).
    writeFileSync(join(dataDir, 'faktor-plus.json'), readFileSync(liveConfig));
  }
  const child = spawn(
    requireFile(BIN, 'faktor binary'),
    ['serve', '--port', '0', '--data-dir', dataDir],
    {
      env: { ...process.env, ...env, FAKTOR_SERVER_PASSWORD: password },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );
  const port = await new Promise((resolve, reject) => {
    let out = '';
    const timer = setTimeout(() => reject(new Error(`daemon did not start: ${out.slice(-800)}`)), 30_000);
    child.stdout.on('data', (d) => {
      out += String(d);
      const m = /listening on http:\/\/127\.0\.0\.1:(\d+)/.exec(out);
      if (m) {
        clearTimeout(timer);
        resolve(Number(m[1]));
      }
    });
    child.on('exit', (code) => {
      clearTimeout(timer);
      reject(new Error(`daemon exited early (${code}): ${out.slice(-800)}`));
    });
  });
  let stderr = '';
  child.stderr.on('data', (d) => {
    stderr = (stderr + String(d)).slice(-8000);
  });
  const daemon = {
    dir,
    dataDir,
    port,
    password,
    child,
    stderrTail: () => stderr,
    stop: () => {
      try {
        child.kill('SIGKILL');
      } catch {
        /* gone */
      }
      rmSync(dir, { recursive: true, force: true });
    },
  };
  CURRENT_DAEMON = daemon;
  return daemon;
}

async function waitReady(daemon) {
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    const res = await httpJson({
      port: daemon.port,
      method: 'GET',
      path: '/native/ready',
      token: daemon.password,
    }).catch(() => null);
    if (res !== null && res.status === 200 && res.json?.ready === true) return;
    await sleep(250);
  }
  fail('daemon never became ready');
}

async function createSession(daemon) {
  const res = await httpJson({
    port: daemon.port,
    method: 'POST',
    path: '/native/session',
    token: daemon.password,
    body: { provider: 'deepseek', model: 'deepseek-chat', workspace: process.cwd(), title: 'cert' },
  });
  if (res.status !== 200 || typeof res.json?.id !== 'string') {
    fail(`session create: ${res.status} ${res.text.slice(0, 300)}`);
  }
  return res.json.id;
}

async function uploadAttachment(daemon, sessionId, bytes, mime, filename) {
  const res = await httpJson({
    port: daemon.port,
    method: 'POST',
    path: `/native/session/${sessionId}/attachments`,
    token: daemon.password,
    // The strict upload DTO: mime + optional filename + the bytes. The
    // digest is derived by the daemon (CAS-addressed); sending extra members
    // is a plain 400 by design.
    body: { mime, filename, data_base64: bytes.toString('base64') },
  });
  if (res.status !== 200 || typeof res.json?.digest !== 'string') {
    fail(`attachment upload: ${res.status} ${res.text.slice(0, 400)}`);
  }
  // The task-admission projection of the uploaded reference (the exact
  // AttachmentId the prompt body carries).
  return {
    digest: res.json.digest,
    mime: res.json.mime,
    filename: res.json.filename ?? null,
    size: res.json.size,
  };
}

async function askWithImage(daemon, sessionId) {
  const png = magentaPng();
  const attachment = await uploadAttachment(daemon, sessionId, png, 'image/png', basename('cert.png'));
  const live = subscribeLive({ port: daemon.port, token: daemon.password, sessionId });
  const prompt = 'What colour dominates the attached image? Answer in one word.';
  const res = await httpJson({
    port: daemon.port,
    method: 'POST',
    path: `/native/session/${sessionId}/prompt`,
    token: daemon.password,
    body: {
      session_id: sessionId,
      submission_id: randomUUID(),
      prompt,
      execution: 'ask',
      attachments: [attachment],
    },
  });
  if (res.status !== 200) fail(`prompt with image: ${res.status} ${res.text.slice(0, 300)}`);
  const deadline = Date.now() + 180_000;
  let answer = '';
  while (Date.now() < deadline) {
    const page = await httpJson({
      port: daemon.port,
      method: 'GET',
      path: `/native/messages?session=${sessionId}&limit=20`,
      token: daemon.password,
    });
    const messages = page.json?.messages ?? [];
    for (const message of messages) {
      if (message.role !== 'assistant') continue;
      for (const part of message.parts ?? []) {
        if (typeof part?.data?.text === 'string') answer += part.data.text;
      }
    }
    if (/magenta|pink|purple|fuchsia/i.test(answer)) {
      live.close();
      console.log('CERT OK: attachment reachability — the model named the image colour');
      return;
    }
    if (answer.length > 0) break;
    await sleep(1000);
  }
  live.close();
  const debugPage = await httpJson({
    port: daemon.port,
    method: 'GET',
    path: `/native/messages?session=${sessionId}&limit=20`,
    token: daemon.password,
  }).catch(() => null);
  const roles = (debugPage?.json?.messages ?? []).map((m) => `${m.role}#${m.seq}`);
  const tail = (debugPage?.json?.messages ?? [])
    .slice(-2)
    .map((m) => `${m.role}#${m.seq}:${JSON.stringify(m.parts ?? []).slice(0, 400)}`);
  console.error(`receipt=${JSON.stringify(res.json)} roles=${roles.join(',')}`);
  for (const line of tail) console.error(`tail ${line}`);
  fail(`image question did not carry the image (answer: ${JSON.stringify(answer.slice(0, 200))})`);
}

async function fluidity(daemon, sessionId) {
  const live = subscribeLive({ port: daemon.port, token: daemon.password, sessionId });
  await sleep(500);
  const posted = Date.now();
  const res = await httpJson({
    port: daemon.port,
    method: 'POST',
    path: `/native/session/${sessionId}/prompt`,
    token: daemon.password,
    body: {
      session_id: sessionId,
      submission_id: randomUUID(),
      prompt: 'Count slowly from 1 to 30, one number per line, nothing else.',
      execution: 'ask',
    },
  });
  if (res.status !== 200) fail(`fluidity prompt: ${res.status} ${res.text.slice(0, 300)}`);
  const acceptedOp = typeof res.json?.op_id === 'string' ? res.json.op_id : null;
  const snapshot = live.frames.find((f) => f.event === 'snapshot');
  if (!snapshot) fail('no snapshot handshake frame');
  let streamed = '';
  let firstFrameAt = null;
  let completedAt = null;
  const deadline = Date.now() + 120_000;
  while (Date.now() < deadline) {
    for (const frame of live.frames) {
      if (frame.event === 'live' && frame.data !== null) {
        if (firstFrameAt === null) firstFrameAt = frame.at;
        // Offsets are UTF-8 byte offsets; append whole content only at the
        // expected offset (replay/duplicate frames are ignored).
        if (frame.data.offset === Buffer.byteLength(streamed, 'utf8')) {
          streamed += frame.data.content ?? '';
        }
      }
      if (frame.event === 'turn' && frame.data?.status !== undefined) completedAt = frame.at;
    }
    if (firstFrameAt !== null && streamed.length > 0) break;
    await sleep(100);
  }
  if (firstFrameAt === null) {
    live.close();
    fail('no live frame arrived: the stream is not live (added latency unmeasurable)');
  }
  const addedMs = firstFrameAt;
  if (addedMs > 5000) {
    live.close();
    fail(`first live frame ${addedMs}ms after subscribe (host-added latency bound 5000ms)`);
  }
  // Stop mid-generation: the accepted operation is aborted and the partial
  // text stays. Then the durable transcript must reconcile byte-exactly.
  const abort = await httpJson({
    port: daemon.port,
    method: 'POST',
    path: `/native/session/${sessionId}/abort`,
    token: daemon.password,
    body: acceptedOp === null ? { session_id: sessionId } : { session_id: sessionId, op_id: acceptedOp },
  });
  if (abort.status !== 200) fail(`Stop/abort refused: ${abort.status} ${abort.text.slice(0, 300)}`);
  await sleep(1500);
  live.close();
  const page = await httpJson({
    port: daemon.port,
    method: 'GET',
    path: `/native/messages?session=${sessionId}&limit=20`,
    token: daemon.password,
  });
  const durable = (page.json?.messages ?? [])
    .filter((m) => m.role === 'assistant')
    .flatMap((m) => (m.parts ?? []).map((p) => (typeof p?.data?.text === 'string' ? p.data.text : '')))
    .join('');
  const streamedPrefix = streamed.trimEnd();
  const durableTrimmed = durable.trimEnd();
  if (!durableTrimmed.startsWith(streamedPrefix) && !streamedPrefix.startsWith(durableTrimmed)) {
    fail(
      `streamed/durable divergence: streamed=${JSON.stringify(streamedPrefix.slice(0, 120))} durable=${JSON.stringify(durableTrimmed.slice(0, 120))}`,
    );
  }
  console.log(
    `CERT OK: fluidity — first live frame ${addedMs}ms after subscribe, ${streamed.length} streamed chars, ` +
      `partial preserved after Stop, durable transcript reconciles`,
  );
}

async function main() {
  if (!runImage && !runFluidity) {
    console.log('usage: node apps/vscode/scripts/live-ask-cert.mjs [--image] [--fluidity] [--all]');
    process.exit(2);
  }
  const env = providerEnv();
  const daemon = await startDaemon(env);
  try {
    await waitReady(daemon);
    if (runImage) {
      const sessionId = await createSession(daemon);
      await askWithImage(daemon, sessionId);
    }
    if (runFluidity) {
      const sessionId = await createSession(daemon);
      await fluidity(daemon, sessionId);
    }
  } finally {
    daemon.stop();
  }
}

main().catch((error) => fail(error instanceof Error ? error.message : String(error)));
