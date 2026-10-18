#!/usr/bin/env node
// Deterministic race/ordering/corruption schedule generators.
//
// These do not replace the real fault lanes; they generate the schedules the
// harnesses replay:
//   * settlement: start prompt N+1 at every await point of turn N's tail;
//   * event streams: duplicate, out-of-order, truncated and overflow cursors;
//   * storage: truncation, bit flips, zero-length and length-lie corruptions.
// Every schedule is small, enumerable and seed-stable, so a red run has an
// exact replay index.

export function settlementSchedules(awaitPoints) {
  const schedules = [];
  for (let point = 0; point < awaitPoints; point += 1) {
    schedules.push({
      id: `settle-await-${point}`,
      steps: [
        { actor: 'turn-N', action: 'admit' },
        { actor: 'turn-N', action: 'settle-until', await_point: point },
        { actor: 'turn-N+1', action: 'start-prompt' },
        { actor: 'turn-N', action: 'resume-await', await_point: point },
        { actor: 'turn-N', action: 'finish-tail' },
      ],
      invariant: 'turn-N+1 row is byte-identical at all await points',
    });
  }
  return schedules;
}

export function eventSequences(seed = 1) {
  const sequences = [
    { id: 'duplicate-delivery', ids: [1, 1, 2, 2, 3], expected: 'dedupe-by-id' },
    { id: 'out-of-order', ids: [3, 1, 4, 2], expected: 'total-order-by-id' },
    { id: 'gap', ids: [1, 2, 5], expected: 'gap-visible-not-fabricated' },
    { id: 'truncated-frame', frames: ['data: {"id":1}', 'data: {"id":'], expected: 'typed-parse-refusal' },
    { id: 'overflow-cursor', cursors: ['18446744073709551616', '-1', 'abc'], expected: 'typed-refusal' },
  ];
  const rotated = sequences.map((sequence, index) => ({
    ...sequence,
    index: (index + seed) % sequences.length,
  }));
  return rotated;
}

export function corruptions(bytes) {
  const copy = Buffer.from(bytes);
  const out = [];
  out.push({ id: 'truncate-half', bytes: copy.subarray(0, Math.max(1, Math.floor(copy.length / 2))) });
  out.push({ id: 'truncate-zero', bytes: copy.subarray(0, 0) });
  const bitflip = Buffer.from(copy);
  if (bitflip.length > 0) bitflip[0] ^= 0xff;
  out.push({ id: 'bitflip-first', bytes: bitflip });
  const lengthLie = Buffer.from(copy);
  if (lengthLie.length >= 4) lengthLie.writeUInt32BE(0xffffffff, 0);
  out.push({ id: 'length-lie', bytes: lengthLie });
  const appended = Buffer.concat([copy, Buffer.from([0xde, 0xad, 0xbe, 0xef])]);
  out.push({ id: 'trailing-garbage', bytes: appended });
  return out;
}

export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`schedule selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`schedule selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const schedules = settlementSchedules(12);
  check('one schedule per await point', schedules.length === 12);
  check('every schedule starts N+1 at its own await point', schedules.every((schedule, index) => schedule.steps[1].await_point === index));
  check('schedules are deterministic', JSON.stringify(settlementSchedules(12)) === JSON.stringify(schedules));
  const events = eventSequences(2);
  check('duplicate/out-of-order/truncated/overflow are generated', events.length === 5);
  const corrupt = corruptions(Buffer.from('durable-state'));
  check('corruption classes are distinct', new Set(corrupt.map((entry) => entry.id)).size === corrupt.length);
  check('bit flip changes a byte', corrupt.find((entry) => entry.id === 'bitflip-first').bytes[0] !== 'd'.charCodeAt(0));
  check('truncate-zero is empty', corrupt.find((entry) => entry.id === 'truncate-zero').bytes.length === 0);
  if (failures > 0) {
    console.error(`schedule selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('schedule selftest: PASS');
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  process.exitCode = process.argv.includes('--selftest') ? selftest() : 0;
}
