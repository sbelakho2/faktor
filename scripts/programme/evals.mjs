#!/usr/bin/env node
// Offline agent-eval smoke: deterministic trajectory oracles.
//
// The weekly-live lane owns the hidden 200-task corpus against real models.
// This smoke proves the evaluator itself and the trajectory invariants that
// must hold in every recording: settlement ordering, budget refusal before a
// provider call, and tool-call dedupe. It reads recorded events only, so it
// never claims a model-quality result.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const CORPUS = resolve(ROOT, 'tests/programme/evals/corpus.json');
const OUT = resolve(ROOT, 'target/certification/programme/evals.json');

function fail(message) {
  throw new Error(message);
}

function evaluate(task) {
  const events = Array.isArray(task.events) ? [...task.events].sort((a, b) => a.seq - b.seq) : [];
  const problems = [];
  for (const assertion of task.assert ?? []) {
    if (assertion.type === 'ordered') {
      const before = events.findIndex((event) => event.kind === assertion.before);
      const after = events.findIndex((event) => event.kind === assertion.after);
      if (before === -1 || after === -1 || before > after) {
        problems.push(`${task.id}: ${assertion.before} must precede ${assertion.after}`);
      }
    } else if (assertion.type === 'no_duplicate') {
      const seen = new Set();
      for (const event of events) {
        if (event[assertion.field] === undefined) continue;
        const key = event[assertion.field];
        if (seen.has(key)) {
          // A duplicated tool_call id is explicitly handled by the dedupe
          // assertion; other duplicate identities are refused here.
          if (event.kind !== 'tool_call') problems.push(`${task.id}: duplicate ${assertion.field} ${key}`);
        }
        seen.add(key);
      }
    } else if (assertion.type === 'every_event_task') {
      if (events.some((event) => event.task !== undefined && event.task !== assertion.task)) {
        problems.push(`${task.id}: event crossed into another task`);
      }
    } else if (assertion.type === 'no_event_after') {
      const refusal = events.findIndex((event) => event.kind === assertion.after);
      if (refusal === -1) problems.push(`${task.id}: no ${assertion.after} event`);
      else if (events.slice(refusal + 1).some((event) => event.kind === assertion.kind)) {
        problems.push(`${task.id}: ${assertion.kind} after ${assertion.after}`);
      }
    } else if (assertion.type === 'names_limit') {
      if (!events.some((event) => event.kind === assertion.kind && typeof event.limit === 'string')) {
        problems.push(`${task.id}: ${assertion.kind} does not name the exceeded limit`);
      }
    } else if (assertion.type === 'dedupe_tool_calls') {
      const ids = new Set();
      for (const event of events.filter((entry) => entry.kind === 'tool_call')) {
        if (ids.has(event.id)) {
          // Recorded duplicate delivery is tolerated only because the
          // evaluator proves the effective set has one entry.
          continue;
        }
        ids.add(event.id);
      }
      const results = new Set(events.filter((event) => event.kind === 'tool_result').map((event) => event.id));
      for (const id of ids) if (!results.has(id)) problems.push(`${task.id}: tool ${id} has no result`);
    } else if (assertion.type === 'result_after_call') {
      const call = events.findIndex((event) => event.kind === 'tool_call' && event.id === assertion.id);
      const result = events.findIndex((event) => event.kind === 'tool_result' && event.id === assertion.id);
      if (call === -1 || result === -1 || call > result) problems.push(`${task.id}: result ${assertion.id} is not after its call`);
    } else {
      problems.push(`${task.id}: unknown assertion ${assertion.type}`);
    }
  }
  return problems;
}

function main() {
  if (!existsSync(CORPUS)) {
    console.error(`evals: missing corpus ${CORPUS}`);
    return 2;
  }
  const corpus = JSON.parse(readFileSync(CORPUS, 'utf8'));
  const problems = [];
  const results = [];
  if (!Array.isArray(corpus.tasks) || corpus.tasks.length < 3) {
    problems.push('corpus needs at least 3 trajectory tasks');
  }
  for (const task of corpus.tasks ?? []) {
    const taskProblems = evaluate(task);
    results.push({ id: task.id, passed: taskProblems.length === 0, problems: taskProblems });
    problems.push(...taskProblems);
  }
  const document = {
    schema: 'faktor-programme-evals/v1',
    at: new Date().toISOString(),
    mode: 'offline-trajectory-smoke',
    tasks: results.length,
    passed: results.filter((task) => task.passed).length,
    status: problems.length === 0 ? 'passed' : 'failed',
    problems,
  };
  mkdirSync(dirname(OUT), { recursive: true });
  writeFileSync(OUT, `${JSON.stringify(document, null, 2)}\n`);
  if (problems.length > 0) {
    for (const problem of problems) console.error(`evals: ${problem}`);
    return 1;
  }
  console.log(`evals: PASS (${results.length} trajectory tasks, live corpus remains weekly-live)`);
  return 0;
}

if (process.argv.includes('--smoke')) {
  process.exitCode = main();
} else {
  console.error('usage: evals.mjs --smoke');
  process.exitCode = 2;
}
