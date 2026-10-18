#!/usr/bin/env node
// Bounded UI delta-path performance gate.
//
// Four workloads, each with a bound that FAILS an O(n^2) or unbounded path:
//
//   1. refresh coalescing: 100 vs 10,000 event frames must produce the SAME
//      small number of refresh runs (one per coalescing window), i.e. per-
//      delta work is O(1), not O(deltas).
//   2. transcript reduction: 500 vs 1,000 messages must stay capped at
//      MAX_TRANSCRIPT_ENTRIES and scale ~linearly (ratio bound, not O(n^2)).
//   3. permission/agent summaries: 100 vs 2,000 served rows must produce
//      bounded output (MAX_PERMISSION_SUMMARIES / MAX_SUMMARY_ARRAY) and
//      linear scan time.
//   4. edition5 patch build: 10 vs 200 staged controls must scale linearly
//      and never sort/rework the whole registry per delta.
//
// Timings use a warmup + median-of-5. The ratio bounds are deliberately
// generous (CI noise), while the O(n^2) signature (4x/100x) still fails.
//
// Output: target/certification/edition5/perf-delta.raw.json.

import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  MAX_PERMISSION_SUMMARIES,
  MAX_SUMMARY_ARRAY,
  MAX_TRANSCRIPT_ENTRIES,
  permissionSummaries,
  boundJson,
  transcriptFromMessages,
} from '../../apps/vscode/src/state.ts';
import { CoalescingRefreshScheduler } from '../../apps/vscode/src/refreshScheduler.ts';
import { E5_CONTROLS } from '../../apps/vscode/src/generated/edition5Registry.ts';
import { bindingOf } from '../../apps/vscode/src/edition5/bindings.ts';
import { buildPatch, EMPTY_DRAFT, stageControl } from '../../apps/vscode/src/edition5/view.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition5');

function median(values) {
  const sorted = [...values].sort((left, right) => left - right);
  return sorted[Math.floor(sorted.length / 2)];
}

function measure(fn, runs = 5) {
  fn();
  const samples = [];
  for (let index = 0; index < runs; index += 1) {
    const start = performance.now();
    fn();
    samples.push(performance.now() - start);
  }
  return median(samples);
}

function assert(condition, message) {
  if (!condition) throw new Error(message);
}

const results = [];

// 1. Coalescing: N frames -> one refresh run per window.
function coalesce(frameCount) {
  let timer = null;
  let runs = 0;
  let callback = null;
  const timers = {
    setTimeout(fn) {
      callback = fn;
      timer = 1;
      return timer;
    },
    clearTimeout() {
      timer = null;
    },
  };
  const scheduler = new CoalescingRefreshScheduler(
    (done) => {
      runs += 1;
      done();
    },
    timers,
    120,
  );
  for (let index = 0; index < frameCount; index += 1) scheduler.request();
  // All frames in one window coalesce to ONE refresh.
  const fire = callback;
  callback = null;
  timer = null;
  if (fire !== null) fire();
  scheduler.dispose();
  return runs;
}

const smallRuns = coalesce(100);
const largeRuns = coalesce(10_000);
results.push({
  id: 'refresh-coalescing',
  passed: largeRuns === smallRuns && smallRuns <= 100,
  detail: `${smallRuns} refresh runs for 100 deltas vs ${largeRuns} for 10,000 (bounded, O(1) per delta)`,
  metric: { small_runs: smallRuns, large_runs: largeRuns },
});

// 2. Transcript reduction: capped and linear.
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
const smallEntries = transcriptFromMessages(smallMessages);
const largeEntries = transcriptFromMessages(largeMessages);
const transcriptSmallMs = measure(() => transcriptFromMessages(smallMessages));
const transcriptLargeMs = measure(() => transcriptFromMessages(largeMessages));
const transcriptRatio = transcriptLargeMs / Math.max(transcriptSmallMs, 0.01);
results.push({
  id: 'transcript-reduction',
  passed:
    smallEntries.length <= MAX_TRANSCRIPT_ENTRIES &&
    largeEntries.length <= MAX_TRANSCRIPT_ENTRIES &&
    transcriptRatio <= 4.0,
  detail: `${smallEntries.length}/${largeEntries.length} entries for 500/1000 messages; ratio ${transcriptRatio.toFixed(2)}x (bound 4.0x)`,
  metric: {
    small_ms: transcriptSmallMs,
    large_ms: transcriptLargeMs,
    ratio: transcriptRatio,
    cap: MAX_TRANSCRIPT_ENTRIES,
  },
});

// 3. Permission/agent summaries: bounded output, linear scan.
const permissions = Array.from({ length: 2000 }, (_, index) => ({
  id: String(index + 1),
  session_id: '7',
  capability: index % 2 === 0 ? 'shell' : 'file_read',
  detail: JSON.stringify({ command: `echo ${index}` }),
}));
const smallPermissions = permissionSummaries(permissions.slice(0, 100));
const largePermissions = permissionSummaries(permissions);
const permissionSmallMs = measure(() => permissionSummaries(permissions.slice(0, 100)));
const permissionLargeMs = measure(() => permissionSummaries(permissions));
const permissionRatio = permissionLargeMs / Math.max(permissionSmallMs, 0.01);
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
const largeAgents = boundJson(agents);
const agentArray = Array.isArray(largeAgents) ? largeAgents : [];
results.push({
  id: 'bounded-summaries',
  passed:
    smallPermissions.length <= MAX_PERMISSION_SUMMARIES &&
    largePermissions.length <= MAX_PERMISSION_SUMMARIES &&
    agentArray.length <= MAX_SUMMARY_ARRAY &&
    permissionRatio <= 6.0,
  detail: `${smallPermissions.length}/${largePermissions.length} permission rows (cap ${MAX_PERMISSION_SUMMARIES}), ${agentArray.length} bounded agent rows (cap ${MAX_SUMMARY_ARRAY}); ratio ${permissionRatio.toFixed(2)}x (bound 6.0x)`,
  metric: {
    small_permission_ms: permissionSmallMs,
    large_permission_ms: permissionLargeMs,
    ratio: permissionRatio,
    permission_cap: MAX_PERMISSION_SUMMARIES,
    agent_cap: MAX_SUMMARY_ARRAY,
  },
});

// 4. Edition5 patch build: linear in staged controls.
function stageControls(count) {
  let draft = EMPTY_DRAFT;
  let staged = 0;
  for (const control of E5_CONTROLS) {
    if (staged >= count) break;
    const binding = bindingOf(control.id);
    if (binding.input === 'readonly' || binding.input === 'action' || binding.configKey === null) {
      continue;
    }
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
const patchSmallMs = measure(() => buildPatch(smallStage.draft, '1'));
const patchLargeMs = measure(() => buildPatch(largeStage.draft, '1'));
const patchRatio = patchLargeMs / Math.max(patchSmallMs, 0.01);
const perControlSmall = patchSmallMs / Math.max(smallStage.staged, 1);
const perControlLarge = patchLargeMs / Math.max(largeStage.staged, 1);
results.push({
  id: 'edition5-patch',
  passed:
    largePatch.patch.set.length === largeStage.staged &&
    patchRatio <= 40.0 &&
    perControlLarge <= perControlSmall * 4.0 + 1.0,
  detail: `${smallStage.staged}/${largeStage.staged} controls -> ${smallPatch.patch.set.length}/${largePatch.patch.set.length} patch rows; ratio ${patchRatio.toFixed(2)}x, per-control ${perControlSmall.toFixed(4)}/${perControlLarge.toFixed(4)}ms`,
  metric: {
    small_ms: patchSmallMs,
    large_ms: patchLargeMs,
    ratio: patchRatio,
    per_control_small_ms: perControlSmall,
    per_control_large_ms: perControlLarge,
    staged_small: smallStage.staged,
    staged_large: largeStage.staged,
  },
});

const failed = results.filter((row) => row.passed !== true);
mkdirSync(OUT, { recursive: true });
writeFileSync(
  resolve(OUT, 'perf-delta.raw.json'),
  `${JSON.stringify(
    {
      schema: 'faktor-edition5-perf-delta/v1',
      generated_at: new Date().toISOString(),
      counts: { workloads: results.length, passed: results.length - failed.length, failed: failed.length },
      results,
    },
    null,
    2,
  )}\n`,
);
for (const row of failed) console.error(`FAIL ${row.id}: ${row.detail}`);
console.log(
  `perf delta: ${results.length - failed.length}/${results.length} bounded workloads passed ` +
    `(${results.map((row) => row.id).join(', ')})`,
);
if (failed.length > 0) process.exit(1);
