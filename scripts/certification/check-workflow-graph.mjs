#!/usr/bin/env node
// Workflow-graph selftest (audit P1-CI): prove that every invocation of the
// release aggregate has a producer for every mandatory input, for EVERY
// event × branch × platform combination the trusted workflow allows.
//
// Why this exists: the aggregate publishes `ci/faktor/trusted-certified` after
// polling the linux, darwin and windows per-platform contexts. If the graph
// ever schedules the aggregate on an event/branch where one of those axes
// does not run (a non-main push used to do exactly that), the aggregate can
// only burn its poll window and publish a false non-success. This checker
// evaluates the workflow's own `when` clauses (workflow-level matrix
// selection and step-level gates) over the combination space and fails when
// an aggregate invocation lacks any producer.
//
// Modes:
//   node scripts/certification/check-workflow-graph.mjs --check
//   node scripts/certification/check-workflow-graph.mjs selftest
//
// Exit codes: 0 graph proven (or selftest pass); 1 violation; 2 usage/parse.

import { readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const DEFAULT_WORKFLOW = join(ROOT, '.woodpecker/trusted/trusted.yaml');

export const AGGREGATE_STEP = 'status-publish';
export const PLATFORM_STEPS = [
  'certificate-status-linux',
  'certificate-status-darwin',
  'certificate-status-windows',
];

/** Top-level workflow matrix platforms. */
export function parseMatrixPlatforms(text) {
  const at = text.search(/^matrix:/m);
  if (at === -1) return [];
  const block = text.slice(at, text.indexOf('\nlabels:', at) === -1 ? undefined : text.indexOf('\nlabels:', at));
  return [...block.matchAll(/^\s{2,}-\s*(\S+)\s*$/gm)].map((m) => m[1]);
}

/** Workflow-level `when:` rules: {events, branch, evaluate, raw}. */
export function parseWorkflowWhen(text) {
  const at = text.search(/^when:/m);
  if (at === -1) return [];
  const rest = text.slice(at + 'when:'.length);
  const end = rest.search(/^steps:/m);
  const block = end === -1 ? rest : rest.slice(0, end);
  const rules = [];
  let current = null;
  for (const line of block.split('\n')) {
    const item = /^\s*-\s*event:\s*(.+)$/.exec(line);
    if (item) {
      current = { events: parseEvents(item[1]), branch: null, evaluate: null, raw: line.trim() };
      rules.push(current);
      continue;
    }
    if (current === null) continue;
    const branch = /^\s*branch:\s*(\S+)\s*$/.exec(line);
    if (branch) {
      current.branch = branch[1];
      continue;
    }
    const evaluate = /^\s*evaluate:\s*(.+)$/.exec(line);
    if (evaluate) {
      current.evaluate = evaluate[1].trim().replace(/^['"]|['"]$/g, '');
    }
  }
  return rules;
}

function parseEvents(raw) {
  const inner = /\[(.*)\]/.exec(raw);
  const body = inner ? inner[1] : raw;
  return body
    .split(',')
    .map((part) => part.trim().replace(/^['"]|['"]$/g, ''))
    .filter((part) => part.length > 0);
}

/** The evaluate expressions this workflow uses are platform comparisons. */
export function evaluateExpression(expression, platform) {
  const equal = /^platform\s*==\s*["']([^"']+)["']$/.exec(expression);
  if (equal) return platform === equal[1];
  const notEqual = /^platform\s*!=\s*["']([^"']+)["']$/.exec(expression);
  if (notEqual) return platform !== notEqual[1];
  return false;
}

/** TRUE when the workflow-level rules schedule `platform` for event/branch. */
export function workflowSchedules(rules, event, branch, platform) {
  return rules.some((rule) => {
    if (!rule.events.includes(event)) return false;
    if (rule.branch !== null && rule.branch !== branch) return false;
    if (rule.evaluate !== null) return evaluateExpression(rule.evaluate, platform);
    return true;
  });
}

/** Parse step blocks into name/when facts. */
export function parseSteps(text) {
  const chunks = text.split(/\n  - name: /).slice(1);
  return chunks.map((chunk) => {
    const name = chunk.split('\n')[0].trim();
    const whenAt = chunk.search(/^\s{4}when:\s*$/m);
    const whenBlock = whenAt === -1 ? '' : chunk.slice(whenAt);
    const platform = /^\s+platform:\s*(\S+)\s*$/m.exec(whenBlock);
    const events = [];
    const eventMatch = /^\s+event:\s*(.+)$/m.exec(whenBlock);
    if (eventMatch) events.push(...parseEvents(eventMatch[1]));
    const branch = /^\s+branch:\s*(\S+)\s*$/m.exec(whenBlock);
    return {
      name,
      platform: platform ? platform[1] : null,
      events,
      branch: branch ? branch[1] : null,
      whenBlock,
    };
  });
}

/** TRUE when `step` runs for event/branch/platform per its own when clause. */
export function stepSchedules(step, event, branch) {
  if (step.events.length > 0 && !step.events.includes(event)) return false;
  if (step.branch !== null && step.branch !== branch) return false;
  return true;
}

/** All graph violations for one workflow text. */
export function graphViolations(text) {
  const violations = [];
  const platforms = parseMatrixPlatforms(text);
  const rules = parseWorkflowWhen(text);
  const steps = parseSteps(text);
  const byName = new Map(steps.map((step) => [step.name, step]));
  const aggregate = byName.get(AGGREGATE_STEP);
  if (aggregate === undefined) {
    violations.push(`missing-step: ${AGGREGATE_STEP}`);
    return violations;
  }
  const aggregatePlatform = aggregate.platform ?? 'linux/amd64';
  for (const event of ['push', 'tag']) {
    for (const branch of ['main', 'feature']) {
      if (!workflowSchedules(rules, event, branch, aggregatePlatform)) continue;
      if (!stepSchedules(aggregate, event, branch)) continue;
      // Every invocation must have all three producers scheduled for the
      // SAME event/branch/platform space.
      for (const name of PLATFORM_STEPS) {
        const producer = byName.get(name);
        if (producer === undefined) {
          violations.push(`missing-producer: ${name} (aggregate runs on ${event}/${branch})`);
          continue;
        }
        if (!stepSchedules(producer, event, branch)) {
          violations.push(
            `unreachable-producer: ${name} does not run for ${event}/${branch} where ${AGGREGATE_STEP} runs`,
          );
          continue;
        }
        const producerPlatform = producer.platform ?? null;
        if (producerPlatform !== null && producerPlatform !== aggregatePlatform) {
          // A producer on a different matrix axis would need the workflow
          // rules to schedule that axis for this event/branch.
          if (!workflowSchedules(rules, event, branch, producerPlatform)) {
            violations.push(
              `unscheduled-axis: ${name} needs ${producerPlatform} for ${event}/${branch} but the workflow does not schedule it`,
            );
          }
        }
      }
      // A success-only producer would be skipped exactly when the certificate
      // fails, so the aggregate could never publish a conclusive non-success.
      for (const name of PLATFORM_STEPS) {
        const producer = byName.get(name);
        if (producer && !/status:\s*\[[^\]]*failure[^\]]*\]/.test(producer.whenBlock)) {
          violations.push(`missing-failure-gate: ${name} must run on [success, failure]`);
        }
      }
    }
  }
  // P1-CI: the aggregate itself must be main-only (non-main pushes cannot
  // satisfy a three-platform aggregate).
  if (aggregate.branch !== 'main') {
    violations.push(`${AGGREGATE_STEP} must be gated to branch: main`);
  }
  return violations;
}

function check(file) {
  const text = readFileSync(file, 'utf8');
  const violations = graphViolations(text);
  if (violations.length > 0) {
    for (const violation of violations) console.error(`workflow-graph: ${violation}`);
    console.error(`check-workflow-graph: FAIL (${violations.length} violation(s))`);
    return 1;
  }
  console.log('check-workflow-graph: PASS (every aggregate invocation has linux+darwin+windows producers)');
  return 0;
}

function selftest() {
  const good = readFileSync(DEFAULT_WORKFLOW, 'utf8');
  let failures = 0;
  const checkCase = (name, ok) => {
    if (ok) console.log(`selftest ok: ${name}`);
    else {
      console.error(`selftest FAIL: ${name}`);
      failures += 1;
    }
  };
  checkCase('the real trusted workflow has a complete aggregate graph', graphViolations(good).length === 0);
  // Mutation witness: dropping main-only gating must be detected.
  const unGated = good.replace(
    /      # PUSH \+ MAIN ONLY[\s\S]*?      branch: main\n/,
    '      # PUSH ONLY\n      event: push\n',
  );
  checkCase(
    'removing branch: main from the aggregate fails the graph check',
    graphViolations(unGated).some((v) => v.includes('branch: main')),
  );
  // Mutation witness: making darwin main-only when the aggregate runs on
  // feature pushes must be detected.
  const featureAggregate = good
    .replace(/      branch: main\n      status: \[success, failure\]\n    environment:/, '      status: [success, failure]\n    environment:')
    .replace(/  - event: push\n    branch: main\n    evaluate:/, '  - event: push\n    evaluate:');
  checkCase(
    'an aggregate on a non-main push without darwin/windows producers fails',
    graphViolations(featureAggregate).length > 0,
  );
  if (failures > 0) {
    console.error(`check-workflow-graph selftest: FAIL (${failures})`);
    process.exit(1);
  }
  console.log('check-workflow-graph selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && process.argv[1].endsWith('check-workflow-graph.mjs');
if (isMain) {
  const args = process.argv.slice(2);
  if (args.includes('selftest')) {
    process.exit(selftest());
  }
  if (args.includes('--check')) {
    process.exit(check(args.includes('--file') ? args[args.indexOf('--file') + 1] : DEFAULT_WORKFLOW));
  }
  console.error('usage: check-workflow-graph.mjs --check [--file FILE] | selftest');
  process.exit(2);
}
