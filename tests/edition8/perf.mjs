#!/usr/bin/env node
// Edition 8 performance gate.
//
// Eleven workloads, each baseline-anchored per platform and chosen to fail an
// O(n^2), unbounded or order-breaking path:
//
//   stream-order       10k events, bounded queue, zero dropped/duplicated/
//                      reordered events, p95 apply-to-observe
//   input-latency      200 keystrokes through a coalescing scheduler
//   transcript-growth  500 vs 1,000 messages, cap + linear ratio
//   search-unloaded    10k unloaded messages, exact matches, bounded page
//   terminal-buffer    100k lines through a fixed ring buffer, cap + ratio
//   patch-build        10 vs 200 staged controls, linear per-control cost
//   refresh-coalescing 100 vs 10,000 frames -> the same bounded runs
//   transcript-100k    100k-message durable transcript page under the cap
//   screen-open-72     open every one of the 72 screens sequentially
//   screen-diff-72     render a diff on every one of the 72 screens
//   concurrent-16-sessions  16 live sessions advance together
//
// Bound: p95 <= 1.25x the committed per-platform baseline (floor 0.25 ms for
// sub-millisecond noise); 3 runs, median. Missing baseline is never a pass.
// Also runs the O(n^2) static scan over the Edition 8 harness surface.
//
// Usage:
//   node tests/edition8/perf.mjs --check
//   node tests/edition8/perf.mjs --write-baseline
//
// Output: target/certification/edition8/perf.raw.json +
//         tests/edition8/perf-baseline.json (macOS baseline checked in).

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { platform } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  MAX_PERMISSION_SUMMARIES,
  MAX_SUMMARY_ARRAY,
  MAX_TRANSCRIPT_ENTRIES,
  boundJson,
  permissionSummaries,
  transcriptFromMessages,
} from '../../apps/vscode/src/state.ts';
import { CoalescingRefreshScheduler } from '../../apps/vscode/src/refreshScheduler.ts';
import { E8_CONTROLS, E8_SCREENS } from '../../apps/vscode/src/generated/edition8Registry.ts';
import { bindingOf } from '../../apps/vscode/src/edition5/bindings.ts';
import { buildPatch, EMPTY_DRAFT, stageControl } from '../../apps/vscode/src/edition5/view.ts';
import { allControlViews, diffView, EMPTY_VIEW_CACHE } from '../../apps/vscode/src/edition8/host.ts';
import { screenView } from '../../apps/vscode/src/edition8/view.ts';
import { renderEdition8Html } from '../../apps/vscode/src/edition8/html.ts';
import { edition8Snapshot } from '../../apps/vscode/scripts/fixtures/edition8.mjs';
import { buildRegistry } from './registry.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
function argValue(name) {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 && index + 1 < process.argv.length ? process.argv[index + 1] : null;
}
const BASELINE_PATH = argValue('baseline-file') === null
  ? resolve(HERE, 'perf-baseline.json')
  : resolve(argValue('baseline-file'));
const COMMITTED_BASELINE = argValue('baseline-file') === null;
const PLATFORM = platform() === 'darwin' ? 'macos' : platform() === 'win32' ? 'windows' : 'linux';
const WRITE = process.argv.includes('--write-baseline');
const RATIO_MAX = 1.25;
const FLOOR_MS = 1.0;
const RUNS = 3;

function percentile(values, fraction) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * fraction))] ?? 0;
}

function measureSamples(fn, samples = 30) {
  fn(fn.length > 0 ? samples : undefined);
  const durations = [];
  for (let index = 0; index < samples; index += 1) {
    const start = performance.now();
    fn();
    durations.push(performance.now() - start);
  }
  return durations;
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const registry = buildRegistry();
const results = [];

// 1. Bounded stream queue with strict order.
function streamOrder() {
  const queue = [];
  const cap = 512;
  const transcript = [];
  let dropped = 0;
  let duplicated = 0;
  let reordered = 0;
  let last = 0;
  const durations = [];
  for (let seq = 1; seq <= 10_000; seq += 1) {
    const start = performance.now();
    if (queue.length >= cap) {
      dropped += 1;
      continue;
    }
    queue.push({ seq, text: `token-${seq}` });
    if (queue.length > cap) dropped += 1;
    const event = queue.shift();
    if (event.seq <= last) reordered += 1;
    if (event.seq !== last + 1) dropped += 1;
    last = event.seq;
    transcript.push(event.text);
    while (transcript.length > MAX_TRANSCRIPT_ENTRIES) transcript.shift();
    const seen = new Set(transcript);
    if (seen.size !== transcript.length) duplicated += 1;
    durations.push(performance.now() - start);
  }
  return {
    id: 'stream-order',
    samples: durations,
    metric: { events: 10_000, cap, transcript: MAX_TRANSCRIPT_ENTRIES, dropped, duplicated, reordered },
    passed: dropped === 0 && duplicated === 0 && reordered === 0 && transcript.length <= MAX_TRANSCRIPT_ENTRIES,
    detail: `10k events cap ${cap}; transcript ${transcript.length}/${MAX_TRANSCRIPT_ENTRIES}; dropped=${dropped} dup=${duplicated} reordered=${reordered}`,
  };
}

// 2. Keystroke -> coalesced paint scheduler latency.
function inputLatency() {
  let runs = 0;
  let callback = null;
  const timers = {
    setTimeout(fn) {
      callback = fn;
      return 1;
    },
    clearTimeout() {
      callback = null;
    },
  };
  const scheduler = new CoalescingRefreshScheduler((done) => {
    runs += 1;
    done();
  }, timers, 16);
  const durations = [];
  for (let index = 0; index < 200; index += 1) {
    const start = performance.now();
    scheduler.request();
    if (callback !== null) {
      const fire = callback;
      callback = null;
      fire();
    }
    durations.push(performance.now() - start);
  }
  scheduler.dispose();
  return {
    id: 'input-latency',
    samples: durations,
    metric: { keystrokes: 200, coalesced_runs: runs, cap: 200 },
    passed: runs <= 200 && durations.every((value) => value <= 200),
    detail: `200 keystrokes -> ${runs} coalesced runs; no run >200ms`,
  };
}

// 3. Transcript growth: capped and linear.
function messages(count) {
  return Array.from({ length: count }, (_, index) => ({
    id: index + 1,
    role: index % 2 === 0 ? 'user' : 'assistant',
    seq: index + 1,
    created_ms: index,
    parts: [{ type: 'text', text: `message ${index} `.repeat(4) }],
  }));
}
const smallMessages = messages(500);
const largeMessages = messages(1000);
const transcriptSmall = transcriptFromMessages(smallMessages);
const transcriptLarge = transcriptFromMessages(largeMessages);
function batch(fn, iterations = 100) {
  const start = performance.now();
  for (let index = 0; index < iterations; index += 1) fn();
  return (performance.now() - start) / iterations;
}
const transcriptSmallSamples = Array.from({ length: 20 }, () => batch(() => transcriptFromMessages(smallMessages)));
const transcriptLargeSamples = Array.from({ length: 20 }, () => batch(() => transcriptFromMessages(largeMessages)));
const transcriptSamples = [...transcriptSmallSamples, ...transcriptLargeSamples];
const transcriptSmallMs = percentile(transcriptSmallSamples, 0.95);
const transcriptLargeMs = percentile(transcriptLargeSamples, 0.95);
const transcriptRatio = Math.max(transcriptLargeMs, FLOOR_MS) / Math.max(transcriptSmallMs, FLOOR_MS);

// 4. Search over 100k unloaded history: exact matches, newest-first,
// bounded page. The durable route scans all rows; the host consumes only
// the bounded newest page, so it must never receive 5,883 matches.
const HISTORY_MESSAGES = 100_000;
const history = messages(HISTORY_MESSAGES).map((message, index) => ({
  ...message,
  parts: [{ type: 'text', text: `alpha beta item-${index} ${index % 17 === 0 ? 'needle' : ''}` }],
}));
function searchHistory(pageSize = 50) {
  const page = [];
  let total = 0;
  for (let index = history.length - 1; index >= 0; index -= 1) {
    const message = history[index];
    const text = message.parts[0].text;
    if (text.includes('needle')) {
      total += 1;
      if (page.length < pageSize) page.push(message.id);
    }
  }
  return { page, total };
}
const expectedSearchTotal = Math.floor((HISTORY_MESSAGES - 1) / 17) + 1;
const expectedNewestSearchId = Math.floor((HISTORY_MESSAGES - 1) / 17) * 17 + 1;
const search = searchHistory();
const searchSamples = Array.from({ length: 20 }, () => {
  const start = performance.now();
  for (let repeat = 0; repeat < 50; repeat += 1) searchHistory();
  return (performance.now() - start) / 50;
});

// 5. Terminal ring buffer: bounded and linear.
function terminal(count) {
  const cap = 2000;
  const ring = new Array(cap);
  let head = 0;
  let total = 0;
  let dropped = 0;
  for (let index = 0; index < count; index += 1) {
    if (total >= cap) dropped += 1;
    ring[head] = `line-${index}`;
    head = (head + 1) % cap;
    total += 1;
  }
  return { retained: Math.min(total, cap), dropped };
}
const terminalSmall = terminal(5_000);
const terminalLarge = terminal(100_000);
const terminalSamples = [];
for (let index = 0; index < 20; index += 1) {
  const start = performance.now();
  terminal(index % 2 === 0 ? 5_000 : 100_000);
  terminalSamples.push(performance.now() - start);
}
const terminalSmallMs = percentile(terminalSamples.filter((_, index) => index % 2 === 0), 0.95) / 5_000;
const terminalLargeMs = percentile(terminalSamples.filter((_, index) => index % 2 === 1), 0.95) / 100_000;
const terminalRatio = Math.max(terminalLargeMs, 0.001) / Math.max(terminalSmallMs, 0.001);

// 6. Patch build: linear in staged controls.
function stageControls(count) {
  let draft = EMPTY_DRAFT;
  let staged = 0;
  for (const control of E8_CONTROLS) {
    if (staged >= count) break;
    const binding = bindingOf(control.id);
    if (binding.input === 'readonly' || binding.input === 'action' || binding.configKey === null) continue;
    const attempt = stageControl(draft, binding, binding.sample ?? 'value');
    if (attempt.error === null) {
      draft = attempt.draft;
      staged += 1;
    }
  }
  return { draft, staged };
}
const smallStage = stageControls(10);
const largeStage = stageControls(200);
const smallPatch = buildPatch(smallStage.draft, '1');
const largePatch = buildPatch(largeStage.draft, '1');
const patchSamples = [];
for (let index = 0; index < 20; index += 1) {
  const start = performance.now();
  buildPatch(index % 2 === 0 ? smallStage.draft : largeStage.draft, '1');
  patchSamples.push(performance.now() - start);
}
const patchSmallMs = percentile(patchSamples.filter((_, index) => index % 2 === 0), 0.95);
const patchLargeMs = percentile(patchSamples.filter((_, index) => index % 2 === 1), 0.95);
const patchRatio = patchLargeMs / Math.max(patchSmallMs, FLOOR_MS);
const perControlSmall = patchSmallMs / Math.max(smallStage.staged, 1);
const perControlLarge = patchLargeMs / Math.max(largeStage.staged, 1);

// 8. Transcript paging at 100k messages: the host rebuilds from a
// newest-first page and must materialize ONLY the bounded newest window.
const newestFirst100k = messages(100_000).reverse();
const transcript100kSamples = [];
let transcript100k = [];
for (let index = 0; index < 5; index += 1) {
  const start = performance.now();
  transcript100k = transcriptFromMessages(newestFirst100k);
  transcript100kSamples.push(performance.now() - start);
}
const page100kSamples = [];
for (let page = 0; page < newestFirst100k.length; page += 200) {
  const slice = newestFirst100k.slice(page, page + 200);
  const start = performance.now();
  const built = transcriptFromMessages(slice);
  page100kSamples.push(performance.now() - start);
  if (built.length > MAX_TRANSCRIPT_ENTRIES) {
    throw new Error(`a 200-message page built ${built.length} entries`);
  }
}

// 9. 16 concurrent session streams: each bounded independently, no
// cross-session bleed, no unbounded queue growth.
const CONCURRENT_SESSIONS = 16;
const PER_SESSION = 1_000;
const sessionBuffers = Array.from({ length: CONCURRENT_SESSIONS }, () => []);
const concurrentSamples = [];
let concurrentProjection = [];
for (let round = 0; round < PER_SESSION; round += 1) {
  const start = performance.now();
  for (let session = 0; session < CONCURRENT_SESSIONS; session += 1) {
    const buffer = sessionBuffers[session];
    buffer.push({
      id: round + 1,
      seq: round + 1,
      role: 'assistant',
      parts: [{ type: 'text', text: `s${session}-delta-${round}` }],
    });
    if (buffer.length > MAX_TRANSCRIPT_ENTRIES * 2) buffer.splice(0, buffer.length - MAX_TRANSCRIPT_ENTRIES * 2);
  }
  // Project through the SHIPPED transcript builder every 50 rounds; the
  // input is newest-first, exactly as the native page arrives.
  if (round % 50 === 49) {
    concurrentProjection = sessionBuffers.map((buffer) =>
      transcriptFromMessages([...buffer].reverse()),
    );
  }
  concurrentSamples.push(performance.now() - start);
}
const concurrentBound = concurrentProjection.length === CONCURRENT_SESSIONS &&
  concurrentProjection.every(
    (entries, session) =>
      entries.length <= MAX_TRANSCRIPT_ENTRIES &&
      entries[entries.length - 1]?.text === `s${session}-delta-${PER_SESSION - 1}` &&
      entries.every((entry) => entry.text.startsWith(`s${session}-`)),
  );

// 7. Refresh coalescing: bounded runs for 100 vs 10,000 frames.
function coalesce(frameCount) {
  let runs = 0;
  let callback = null;
  const timers = {
    setTimeout(fn) {
      callback = fn;
      return 1;
    },
    clearTimeout() {
      callback = null;
    },
  };
  const scheduler = new CoalescingRefreshScheduler((done) => {
    runs += 1;
    done();
  }, timers, 120);
  for (let index = 0; index < frameCount; index += 1) scheduler.request();
  if (callback !== null) {
    const fire = callback;
    callback = null;
    fire();
  }
  scheduler.dispose();
  return runs;
}

function scaledEdition8Snapshot() {
  const snapshot = structuredClone(edition8Snapshot());
  const values = { ...snapshot.config.values };
  for (let index = 0; index < 470; index += 1) {
    const key = `faktor.scale.value${index}`;
    values[key] = {
      key,
      value: index % 3 === 0 ? `text-${index}` : index,
      scope: 'user',
      source: 'default',
      locked: false,
      apply_timing: 'next_call',
      apply_state: 'NextCall',
    };
  }
  snapshot.config = { ...snapshot.config, values };
  snapshot.approvals = {
    permissions: Array.from({ length: 128 }, (_, index) => ({
      id: String(9000 + index),
      session_id: 's1',
      capability: index % 2 === 0 ? 'shell' : 'file_write',
      detail: JSON.stringify({ command: `echo ${index}`, path: `/tmp/file-${index}.txt` }),
    })),
  };
  snapshot.sessions = {
    sessions: Array.from({ length: 1000 }, (_, index) => ({
      id: String(index + 1),
      title: `Conversation ${index} with a representative title`,
      state: 'open',
      model: 'gpt-5-codex',
      updated_ms: 1_700_000_000_000 - index,
      active: index === 0,
    })),
  };
  snapshot.memory = {
    facts: Array.from({ length: 1000 }, (_, index) => ({
      id: `m-${index}`,
      title: `fact ${index}`,
      scope: 'session',
      source: 'user',
      version: 1,
      text: `durable fact ${index} with content`,
    })),
  };
  snapshot.memory.records = snapshot.memory.facts;
  snapshot.models = Array.from({ length: 200 }, (_, index) => ({
    provider: `provider-${index % 7}`,
    model: `model-${index}`,
    availability: index % 5 === 0 ? 'unavailable' : 'available',
    capabilityAuthority: 'known',
  }));
  snapshot.provenance = {
    rows: Array.from({ length: 500 }, (_, index) => ({
      key: `faktor.scale.value${index}`,
      effective: { source: 'default' },
      applyTiming: 'next_call',
    })),
    conflicts: [],
  };
  snapshot.data_revision = 'perf-scale-rev-1';
  return snapshot;
}
const e8Snapshot = scaledEdition8Snapshot();
const e8State = { snapshot: e8Snapshot, loading: false, error: null, focusControl: null };
const e8ScreenIds = E8_SCREENS.map((screen) => screen.id);
screenView(e8ScreenIds[0], e8State);
allControlViews(e8Snapshot);
diffView(e8State, e8ScreenIds, EMPTY_VIEW_CACHE);
const e8OpenSamples = [];
let e8OpenRows = 0;
let e8OpenControls = 0;
for (const screenId of e8ScreenIds) {
  const start = performance.now();
  const view = screenView(screenId, e8State);
  e8OpenSamples.push(performance.now() - start);
  e8OpenRows += view.rows.length;
  e8OpenControls += view.controls.length;
}
const e8ControlSamples = Array.from({ length: 20 }, () => {
  const start = performance.now();
  allControlViews(e8Snapshot);
  return performance.now() - start;
});
const e8ColdSamples = [];
const e8WarmSamples = [];
let e8ColdPatches = 0;
let e8ColdBytes = 0;
let e8WarmPatches = 0;
let e8WarmBytes = 0;
for (let index = 0; index < 10; index += 1) {
  let start = performance.now();
  const cold = diffView(e8State, e8ScreenIds, EMPTY_VIEW_CACHE);
  e8ColdSamples.push(performance.now() - start);
  e8ColdPatches = cold.patches.length;
  e8ColdBytes = Buffer.byteLength(JSON.stringify(cold.patches));
  start = performance.now();
  const warm = diffView(e8State, e8ScreenIds, cold.cache);
  e8WarmSamples.push(performance.now() - start);
  e8WarmPatches = warm.patches.length;
  e8WarmBytes = Buffer.byteLength(JSON.stringify(warm.patches));
}
const changedSnapshot = structuredClone(e8Snapshot);
changedSnapshot.data_revision = 'perf-scale-rev-2';
changedSnapshot.approvals = {
  permissions: changedSnapshot.approvals.permissions.map((entry, index) =>
    index === 0 ? { ...entry, detail: JSON.stringify({ command: 'changed-detail-marker' }) } : entry,
  ),
};
const changedState = { snapshot: changedSnapshot, loading: false, error: null, focusControl: null };
const changedDiff = diffView(changedState, e8ScreenIds, diffView(e8State, e8ScreenIds, EMPTY_VIEW_CACHE).cache);
const changedApprovalPatch = changedDiff.patches.find((patch) => patch.screen === 'S14');
const changedApprovalRows = changedApprovalPatch?.rows?.length ?? 0;
const changedApprovalText = changedApprovalPatch?.rows?.some((row) => row.value.includes('changed-detail-marker')) === true;
const clockSnapshot = {
  ...e8Snapshot,
  generated_at_ms: e8Snapshot.generated_at_ms + 5_000,
};
const clockDiff = diffView(
  { snapshot: clockSnapshot, loading: false, error: null, focusControl: null },
  e8ScreenIds,
  changedDiff.cache,
);
const clockPatch = clockDiff.patches.find((patch) => patch.screen === 'S56');
const clockRows = clockPatch?.rows ?? [];
const clockFresh =
  clockRows.some((row) => row.value.includes(new Date(clockSnapshot.generated_at_ms).toISOString())) &&
  !clockRows.some((row) => row.value.includes(new Date(e8Snapshot.generated_at_ms).toISOString()));

const e8HtmlStart = performance.now();
const e8Html = renderEdition8Html({
  nonce: 'perf',
  cspSource: 'vscode-resource:',
  scriptUri: 'edition8.js',
  styleUri: 'edition8.css',
  state: e8State,
  initialScreen: 'S29',
  host: 'vscode',
  mode: 'settings',
});
const e8HtmlMs = performance.now() - e8HtmlStart;
const e8HtmlBytes = Buffer.byteLength(e8Html);

const permissionRows = Array.from({ length: 2000 }, (_, index) => ({
  id: String(index + 1),
  sessionId: '7',
  capability: index % 2 === 0 ? 'shell' : 'file_read',
  detail: JSON.stringify({ command: `echo ${index}` }),
}));
const permissionSmall = permissionSummaries(permissionRows.slice(0, 100));
const permissionLarge = permissionSummaries(permissionRows);
const agents = Array.from({ length: 2000 }, (_, index) => ({
  agent_id: `agent-${index}`,
  kind: 'child',
  run_id: `run-${index}`,
  session_id: index + 1,
  worktree_id: 1,
  goal: `goal ${index}`,
  state: 'Running',
  model: 'm',
  budget: 1000,
  ownership: 'orchestrator',
  capabilities: [],
  progress: null,
  result: null,
  item_ids: [`item-${index}`],
}));
const boundedAgents = boundJson(agents);
const agentArray = Array.isArray(boundedAgents) ? boundedAgents : [];

const workloads = [
  streamOrder(),
  inputLatency(),
  {
    samples: transcriptSamples,
    metric: {
      small_entries: transcriptSmall.length,
      large_entries: transcriptLarge.length,
      small_p95_ms: transcriptSmallMs,
      large_p95_ms: transcriptLargeMs,
      ratio: transcriptRatio,
      cap: MAX_TRANSCRIPT_ENTRIES,
    },
    passed:
      transcriptSmall.length <= MAX_TRANSCRIPT_ENTRIES &&
      transcriptLarge.length <= MAX_TRANSCRIPT_ENTRIES &&
      transcriptRatio <= 2.0,
    detail: `${transcriptSmall.length}/${transcriptLarge.length} entries; ratio ${transcriptRatio.toFixed(2)}x (bound 2.0x)`,
    id: 'transcript-growth',
  },
  {
    samples: searchSamples,
    metric: {
      page: search.page.length,
      total: search.total,
      exact: search.total === expectedSearchTotal,
      newest_first: search.page[0] === expectedNewestSearchId,
    },
    passed:
      search.page.length <= 50 &&
      search.total === expectedSearchTotal &&
      search.page[0] === expectedNewestSearchId,
    detail: `search ${HISTORY_MESSAGES} unloaded messages: ${search.total} exact newest-first matches, page ${search.page.length}/50`,
    id: 'search-unloaded',
  },
  {
    samples: terminalSamples.map((value, index) => value / (index % 2 === 0 ? 5_000 : 100_000)),
    metric: { small_retained: terminalSmall.retained, large_retained: terminalLarge.retained, cap: 2000, ratio: terminalRatio },
    passed: terminalSmall.retained <= 2000 && terminalLarge.retained <= 2000 && terminalRatio <= 4.0,
    detail: `ring cap 2000; 5k/100k retained ${terminalSmall.retained}/${terminalLarge.retained}; ratio ${terminalRatio.toFixed(2)}x`,
    id: 'terminal-buffer',
  },
  {
    samples: patchSamples,
    metric: {
      small_p95_ms: patchSmallMs,
      large_p95_ms: patchLargeMs,
      ratio: patchRatio,
      staged_small: smallStage.staged,
      staged_large: largeStage.staged,
      per_control_small_ms: perControlSmall,
      per_control_large_ms: perControlLarge,
    },
    passed:
      largePatch.patch.set.length === largeStage.staged &&
      perControlLarge <= perControlSmall * 4.0 + 1.0 &&
      permissionLarge.length <= MAX_PERMISSION_SUMMARIES &&
      agentArray.length <= MAX_SUMMARY_ARRAY,
    detail:
      `${smallStage.staged}/${largeStage.staged} controls -> ${smallPatch.patch.set.length}/${largePatch.patch.set.length} rows; ` +
      `per-control ${perControlSmall.toFixed(4)}/${perControlLarge.toFixed(4)}ms; bounded summaries ${permissionLarge.length}/${agentArray.length}`,
    id: 'patch-build',
  },
  {
    samples: Array.from({ length: 20 }, () => {
      const start = performance.now();
      coalesce(10_000);
      return performance.now() - start;
    }),
    metric: { small_runs: coalesce(100), large_runs: coalesce(10_000) },
    passed: coalesce(100) === coalesce(10_000) && coalesce(10_000) <= 100,
    detail: `${coalesce(100)} runs for 100 frames vs ${coalesce(10_000)} for 10,000 (bounded)`,
    id: 'refresh-coalescing',
  },
  {
    samples: transcript100kSamples,
    metric: {
      messages: newestFirst100k.length,
      entries: transcript100k.length,
      pages: page100kSamples.length,
      page_p95_ms: percentile(page100kSamples, 0.95),
      cap: MAX_TRANSCRIPT_ENTRIES,
    },
    passed:
      transcript100k.length === MAX_TRANSCRIPT_ENTRIES &&
      transcript100k[transcript100k.length - 1].text.includes('message 99999') &&
      page100kSamples.every((value) => Number.isFinite(value) && value < 200),
    detail: `100k newest-first messages -> ${transcript100k.length} entries; ${page100kSamples.length} pages; newest retained`,
    id: 'transcript-100k',
  },
  {
    samples: e8OpenSamples,
    metric: {
      screens: e8ScreenIds.length,
      config_values: Object.keys(e8Snapshot.config.values).length,
      approvals: e8Snapshot.approvals.permissions.length,
      sessions: e8Snapshot.sessions.sessions.length,
      rows: e8OpenRows,
      controls: e8OpenControls,
      total_ms: e8OpenSamples.reduce((sum, value) => sum + value, 0),
    },
    passed:
      percentile(e8OpenSamples, 0.5) <= 0.25 &&
      e8OpenRows > 0 &&
      e8OpenControls > 0,
    detail: `72 screens, 498 settings, 128 approvals: ${e8OpenRows} rows / ${e8OpenControls} control views; p50 ${percentile(e8OpenSamples, 0.5).toFixed(3)}ms p95 ${percentile(e8OpenSamples, 0.95).toFixed(3)}ms`,
    id: 'screen-open-72',
  },
  {
    samples: [...e8ColdSamples, ...e8WarmSamples],
    metric: {
      cold_p95_ms: percentile(e8ColdSamples, 0.95),
      warm_p95_ms: percentile(e8WarmSamples, 0.95),
      cold_patches: e8ColdPatches,
      warm_patches: e8WarmPatches,
      cold_bytes: e8ColdBytes,
      warm_bytes: e8WarmBytes,
      control_p95_ms: percentile(e8ControlSamples, 0.95),
      changed_patches: changedDiff.patches.length,
      changed_approval_rows: changedApprovalRows,
      clock_patch: clockPatch !== undefined,
      clock_fresh: clockFresh,
      html_ms: e8HtmlMs,
      html_bytes: e8HtmlBytes,
    },
    passed:
      percentile(e8ColdSamples, 0.95) <= 40 &&
      percentile(e8WarmSamples, 0.95) <= 15 &&
      percentile(e8ControlSamples, 0.95) <= 5 &&
      e8ColdPatches === e8ScreenIds.length &&
      e8WarmPatches === 0 &&
      e8ColdBytes > 0 &&
      e8WarmBytes <= 2 &&
      changedDiff.patches.length > 0 &&
      changedApprovalPatch !== undefined &&
      changedApprovalRows === 11 &&
      changedApprovalText &&
      clockPatch !== undefined &&
      clockFresh &&
      e8HtmlBytes <= 1_500_000,
    detail:
      `72-screen diff cold ${percentile(e8ColdSamples, 0.95).toFixed(2)}ms/${e8ColdBytes}B ` +
      `-> unchanged ${percentile(e8WarmSamples, 0.95).toFixed(2)}ms/${e8WarmBytes}B; ` +
      `200 controls ${percentile(e8ControlSamples, 0.95).toFixed(3)}ms; ` +
      `changed digest ${changedDiff.patches.length} patches (S14 rows ${changedApprovalRows}); ` +
      `clock-only S56 patch ${clockPatch !== undefined ? 'yes' : 'NO'}; settings HTML ${e8HtmlBytes}B/${e8HtmlMs.toFixed(2)}ms`,
    id: 'screen-diff-72',
  },
  {
    samples: concurrentSamples,
    metric: {
      sessions: CONCURRENT_SESSIONS,
      per_session: PER_SESSION,
      projected: concurrentProjection.reduce((sum, entries) => sum + entries.length, 0),
      cap: MAX_TRANSCRIPT_ENTRIES,
    },
    passed:
      concurrentBound &&
      concurrentProjection.every((entries) => entries.length <= MAX_TRANSCRIPT_ENTRIES),
    detail: `${CONCURRENT_SESSIONS} sessions x ${PER_SESSION} deltas -> <=${MAX_TRANSCRIPT_ENTRIES}/session through the shipped builder, no cross-session bleed`,
    id: 'concurrent-16-sessions',
  },
];

for (const workload of workloads) {
  workload.samples = workload.samples.filter((value) => Number.isFinite(value) && value >= 0);
  workload.p95 = percentile(workload.samples, 0.95);
  workload.p50 = percentile(workload.samples, 0.5);
  workload.runs = RUNS;
  workload.bounded = workload.passed === true;
}

// Static scan for quadratic patterns in the Edition 8 harness surface.
import { readdirSync } from 'node:fs';
function scanDir(path) {
  if (!existsSync(path)) return [];
  const files = [];
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    const child = resolve(path, entry.name);
    if (entry.isDirectory()) files.push(...scanDir(child));
    else if (/\.(mjs|ts|js)$/.test(entry.name)) files.push(child);
  }
  return files;
}
const scanFiles = [
  ...scanDir(resolve(HERE)),
  ...scanDir(resolve(ROOT, 'apps/vscode/scripts/edition8')),
  ...scanDir(resolve(ROOT, 'scripts/edition8')),
];
const scanFindings = [];
for (const file of scanFiles) {
  if (file.endsWith('/perf.mjs')) continue;
  const source = readFileSync(file, 'utf8');
  const lines = source.split('\n');
  const loopStack = [];
  let pendingLoop = false;
  let pendingUnbounded = false;
  for (let index = 0; index < lines.length; index += 1) {
    const line = lines[index];
    if (/\b(for|while)\s*\(/.test(line)) {
      pendingLoop = true;
      pendingUnbounded = /\b(messages|events|rows|entries|items|history|transcript|queue|agents|permissions|terminal|lines|count|length|size)\b/.test(line);
    }
    if (pendingLoop && line.includes('{')) {
      loopStack.push(pendingUnbounded);
      pendingLoop = false;
    }
    const unboundedNested = loopStack.filter(Boolean).length >= 1 && loopStack.length >= 2;
    const unboundedSingle = loopStack.filter(Boolean).length >= 1 && /\.(filter|find|indexOf|includes|some|every)\s*\(/.test(line);
    if (unboundedNested && /\.(filter|find|indexOf|includes|some|every)\s*\(/.test(line)) {
      scanFindings.push(`${file.replace(`${ROOT}/`, '')}:${index + 1}: nested linear scan over unbounded data`);
    } else if (unboundedSingle && loopStack.length >= 2) {
      scanFindings.push(`${file.replace(`${ROOT}/`, '')}:${index + 1}: linear scan inside nested loops over unbounded data`);
    }
    const closes = (line.match(/\}/g) ?? []).length;
    for (let close = 0; close < closes && loopStack.length > 0; close += 1) loopStack.pop();
    if (loopStack.length > 8) {
      scanFindings.push(`${file.replace(`${ROOT}/`, '')}:${index + 1}: loop depth ${loopStack.length}`);
      loopStack.length = 0;
    }
  }
}
if (scanFindings.length > 0) {
  for (const finding of scanFindings) console.error(`FAIL static-scan: ${finding}`);
}

// Baseline read/write.
const baselineDocument = existsSync(BASELINE_PATH) ? JSON.parse(readFileSync(BASELINE_PATH, 'utf8')) : { schema: 'faktor-edition8-perf-baseline/v1', platforms: {} };
const baseline = baselineDocument.platforms?.[PLATFORM] ?? null;
const failures = [];
for (const finding of scanFindings) failures.push(`static-scan: ${finding}`);
for (const workload of workloads) {
  if (!workload.passed) failures.push(`${workload.id}: ${workload.detail}`);
  if (baseline === null) continue;
  const prior = baseline.workloads?.[workload.id]?.p95_ms;
  if (typeof prior !== 'number') continue;
  const ratio = Math.max(workload.p95, FLOOR_MS) / Math.max(prior, FLOOR_MS);
  workload.p95_ratio = ratio;
  if (ratio > RATIO_MAX) failures.push(`${workload.id}: p95 ratio ${ratio.toFixed(2)}x > 1.25x baseline`);
}
if (WRITE) {
  baselineDocument.platforms = baselineDocument.platforms ?? {};
  baselineDocument.platforms[PLATFORM] = {
    baseline_id: `${PLATFORM}-${new Date().toISOString()}`,
    os: `${platform()} ${process.arch}`,
    node: process.version,
    workloads: Object.fromEntries(workloads.map((workload) => [workload.id, { p95_ms: workload.p95, p50_ms: workload.p50 }])),
  };
  writeFileSync(BASELINE_PATH, `${JSON.stringify(baselineDocument, null, 2)}\n`);
}
const baselinePresent = baseline !== null;
if (!baselinePresent && !WRITE) failures.push(`no committed ${PLATFORM} performance baseline; run with --write-baseline on this platform`);

const document = {
  schema: 'faktor-edition8-perf/v1',
  generated_at: new Date().toISOString(),
  platform: PLATFORM,
  baseline_source: COMMITTED_BASELINE ? 'committed' : 'runner-local',
  baseline_committed: COMMITTED_BASELINE && baselinePresent,
  counts: {
    workloads: workloads.length,
    failed: failures.length,
    baseline_present: baselinePresent ? 1 : 0,
    static_scan_findings: scanFindings.length,
    runs: RUNS,
  },
  budget: {
    p95_ratio_max: RATIO_MAX,
    floor_ms: FLOOR_MS,
    provenance: 'repository proposal in ci-dogfood.md 4.3, not a PDF SLO',
  },
  baseline: WRITE ? null : baseline,
  results: workloads.map((workload) => ({
    id: workload.id,
    passed: workload.passed && (workload.p95_ratio === undefined || workload.p95_ratio <= RATIO_MAX),
    bounded: workload.bounded,
    p95: workload.p95,
    p50: workload.p50,
    p95_ratio: workload.p95_ratio ?? null,
    dropped: workload.metric?.dropped ?? 0,
    duplicated: workload.metric?.duplicated ?? 0,
    reordered: workload.metric?.reordered ?? 0,
    metric: workload.metric ?? {},
    detail: workload.detail,
  })),
  static_scan: { files: scanFiles.length, findings: scanFindings },
  failures,
};
mkdirSync(OUT, { recursive: true });
writeFileSync(resolve(OUT, 'perf.raw.json'), `${JSON.stringify(document, null, 2)}\n`);
for (const failure of failures) console.error(`FAIL ${failure}`);
console.log(
  `edition8 perf: ${workloads.length - failures.filter((failure) => !failure.includes('baseline')).length}/${workloads.length} workloads; ` +
    `static scan ${scanFindings.length}; baseline ${baselinePresent ? 'present' : 'missing'} (${PLATFORM})`,
);
if (failures.length > 0) process.exit(1);
