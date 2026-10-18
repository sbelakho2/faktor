#!/usr/bin/env node
// Deterministic hostile-input generators for the surface probes.
//
// The report's weakness is not "no auth test"; it is that malformed,
// oversized, duplicate-key, path-traversal and out-of-range inputs were only
// exercised on the happy path of a few routes. These generators are
// seed-stable (same seed => same bytes) so a failure is replayable, and they
// cap their own size so a generator bug cannot OOM the runner.

export const MAX_BODY_BYTES = 512 * 1024;

export function mulberry32(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
  };
}

function nested(depth) {
  let value = 'leaf';
  for (let index = 0; index < depth; index += 1) value = { nested: [value] };
  return value;
}

/**
 * Route/CLI bodies. `raw` is a literal JSON string (duplicate keys cannot be
 * represented by JSON.stringify), otherwise `json` is serialized by the caller.
 */
export function hostileBodies(seed = 0x5eed, count = 8) {
  const start = Math.abs(seed) % 10;
  const cases = [
    {
      name: 'oversized-string',
      json: { value: 'A'.repeat(Math.min(MAX_BODY_BYTES - 64, 256 * 1024)) },
    },
    { name: 'nul-and-controls', json: { value: 'a\u0000b\u0007c\u001f' } },
    { name: 'duplicate-keys', raw: '{"id":1,"id":2,"path":"a","path":"../b"}' },
    { name: 'deep-nesting', json: nested(200) },
    { name: 'wrong-types', json: { id: [], limit: {}, path: 7, approved: 'no' } },
    { name: 'prototype-keys', json: { __proto__: { polluted: true }, constructor: { prototype: { x: 1 } } } },
    { name: 'unicode-boundaries', json: { value: '\u{10ffff}\u{fffd}\u202e\u0000' } },
    { name: 'negative-and-overflow', json: { limit: -1, after: 18446744073709551616, maxTurns: -0.5 } },
    { name: 'empty-object', json: {} },
    { name: 'array-instead-of-object', json: [1, 2, 3] },
  ];
  const out = [];
  for (let index = 0; index < count; index += 1) {
    out.push(cases[(start + index) % cases.length]);
  }
  return out;
}

export function hostileQueries(seed = 0xc0ffee) {
  const random = mulberry32(seed);
  const values = [
    'limit=18446744073709551616',
    'limit=-1',
    'after=%00',
    'cursor=..%2F..%2Fetc%2Fpasswd',
    'path=%2e%2e%2fescape',
    'q=%F0%9F%92%A9%00',
    `limit=${Math.floor(random() * 1e9)}&after=NaN`,
  ];
  return values;
}

export function hostileHeaders(seed = 0xbeef) {
  const random = mulberry32(seed);
  return [
    { 'content-type': 'application/json; charset=utf-16' },
    { 'content-type': 'text/plain' },
    { 'x-request-id': 'r'.repeat(8192) },
    { 'x-request-id': `probe-${Math.floor(random() * 1e12)}` },
  ];
}

export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`hostile selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`hostile selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const first = JSON.stringify(hostileBodies(7));
  const second = JSON.stringify(hostileBodies(7));
  check('bodies are deterministic for a seed', first === second);
  check('a different seed is allowed to differ', JSON.stringify(hostileBodies(8)) !== first);
  const bodies = hostileBodies();
  check('every hostile class is present', new Set(bodies.map((body) => body.name)).size >= 8);
  const bytes = bodies.reduce((sum, body) => {
    const text = body.raw ?? JSON.stringify(body.json);
    return sum + Buffer.byteLength(text);
  }, 0);
  check(`generated bodies stay bounded (${bytes} bytes)`, bytes < 8 * MAX_BODY_BYTES);
  check('duplicate-key case is raw JSON', bodies.some((body) => body.name === 'duplicate-keys' && body.raw));
  check('traversal query is present', hostileQueries().some((query) => query.includes('%2e%2e')));
  check('oversized header is present', hostileHeaders().some((header) => (header['x-request-id'] ?? '').length > 1000));
  if (failures > 0) {
    console.error(`hostile selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('hostile selftest: PASS');
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  process.exitCode = process.argv.includes('--selftest') ? selftest() : 0;
}
