// TEST-SEPARATION: oracle=external-fixture isolation=isolated-browser-profile, read-only-artifacts
// Headless checks for the shared UX state model. These are the mutation
// witnesses' gates: Ask isolation, no-billing-in-run, attention aggregation
// and cause-specific state mapping.

import assert from 'node:assert/strict';
import {
  DEFAULT_COMPOSER_INTENT,
  UX_INVARIANTS,
  aggregateAttention,
  composerIntentOf,
  humanUsd,
  projectUx,
} from '../src/uxState.ts';
import { accountReadPlan, refreshReadPlan } from '../src/refreshReads.ts';

let passed = 0;
function test(label, fn) {
  try {
    fn();
    passed += 1;
    console.log(`PASS  ${label}`);
  } catch (error) {
    console.error(`FAILED  ${label} — ${error && error.message ? error.message : String(error)}`);
    process.exitCode = 1;
  }
}

test('Ask is the default intent and Task is explicit', () => {
  assert.equal(DEFAULT_COMPOSER_INTENT, 'ask');
  assert.equal(composerIntentOf(undefined), 'ask');
  assert.equal(composerIntentOf('ask'), 'ask');
  assert.equal(composerIntentOf('task'), 'task');
  assert.equal(composerIntentOf('TASK'), 'ask', 'only the exact Task intent is accepted');
});

test('credential failure opens provider settings and is grouped once', () => {
  const ux = projectUx({
    machineState: 'failed_recoverable',
    lastError: 'provider returned 401 unauthorized: invalid API key',
    activeRunId: 'run-1',
    task: { goal: 'ship', state: 'running' },
  });
  assert.equal(ux.presentation, 'Failed');
  assert.equal(ux.presentationLabel, 'Provider setup needed');
  assert.equal(ux.presentationAction.key, 'openProviderSettings');
  assert.equal(ux.attention.length, 1);
  assert.equal(ux.attention[0].kind, 'MissingCredential');
  assert.equal(ux.attention[0].action.key, 'openProviderSettings');
});

test('transient provider failure retries without asking the user to resend', () => {
  const ux = projectUx({
    machineState: 'failed_recoverable',
    lastError: 'provider overloaded (503), retrying',
    activeRunId: 'run-1',
    task: { goal: 'ship', state: 'running' },
  });
  assert.equal(ux.presentation, 'Working');
  assert.equal(ux.presentationLabel, 'Retrying automatically');
  assert.doesNotMatch(ux.presentationDetail, /send|resend/i);
  assert.deepEqual(ux.attention, []);
});

test('daemon crash reconnects; blocked stream is a connection interruption', () => {
  const crashed = projectUx({ daemon: 'stopped' });
  assert.equal(crashed.presentationLabel, 'Reconnecting');
  assert.equal(crashed.presentationAction.key, 'reconnect');
  const network = projectUx({ streamStatus: 'protocol_blocked' });
  assert.equal(network.presentationLabel, 'Connection interrupted');
  assert.equal(network.presentationAction.key, 'reconnect');
});

test('durable pause offers Resume and never says resend', () => {
  const ux = projectUx({
    runs: [{ state: 'paused' }],
    task: { state: 'paused', goal: 'ship' },
  });
  assert.equal(ux.presentation, 'Paused');
  assert.equal(ux.presentationLabel, 'Paused');
  assert.equal(ux.presentationAction.key, 'resumeRun');
  assert.doesNotMatch(ux.presentationDetail, /send|resend|submit again/i);
  assert.equal(UX_INVARIANTS.pausedOffersResend, false);
});

test('pre-run failure retries the kept draft', () => {
  const ux = projectUx({
    machineState: 'failed_recoverable',
    lastError: 'admission refused before the run started',
    errorKind: 'run_failure',
    activeRunId: null,
    busy: false,
  });
  assert.equal(ux.presentationLabel, "Couldn't start");
  assert.equal(ux.presentationAction.key, 'retryStart');
});

test('needs-input, failed check and unrecoverable failures have distinct labels', () => {
  const needs = projectUx({ machineState: 'needs_user_input' });
  assert.equal(needs.presentation, 'AwaitingUser');
  assert.equal(needs.presentationLabel, 'Needs your input');
  const failedCheck = projectUx({
    activeRunId: 'run-1',
    verification: { status: 'failed', checksFailed: 2 },
  });
  assert.equal(failedCheck.presentationLabel, 'Checks found a problem');
  assert.equal(failedCheck.presentationAction.key, 'openChecks');
  assert.equal(failedCheck.attention[0].kind, 'FailedCheck');
  // A failed verification with ZERO findings (interrupted before checks ran)
  // surfaces as NOT passed without inventing a "0 checks" finding row.
  const noFindings = projectUx({
    activeRunId: 'run-1',
    verification: { status: 'failed', checksFailed: 0 },
  });
  assert.equal(noFindings.attention.length, 1);
  assert.equal(noFindings.attention[0].kind, 'FailedCheck');
  assert.equal(noFindings.attention[0].label, 'Verification did not pass');
  assert.ok(!/0 checks/.test(noFindings.attention[0].label), 'never a "0 checks" finding');
  assert.ok(noFindings.attention[0].count >= 1, 'the verdict row counts at least one');
  const unrecoverable = projectUx({
    machineState: 'failed',
    lastError: 'durable state is corrupt',
    activeRunId: 'run-1',
  });
  assert.equal(unrecoverable.presentationLabel, "Couldn't continue");
  assert.equal(unrecoverable.presentationAction.key, 'openDiagnostics');
});

test('machine terms never enter the chat projection (Diagnostics reads the snapshot)', () => {
  const ux = projectUx({
    daemon: 'running',
    machineState: 'ready_for_next_turn',
    lastError: 'corrupt_durable_state',
    runs: [{ state: 'Verifying' }],
    proofTaskState: 'verifying',
    streamStatus: 'open',
    activeRunId: 'run-1',
  });
  assert.doesNotMatch(ux.presentationLabel, /failed_recoverable|corrupt_durable_state/);
  assert.ok(!('advanced' in ux), 'the projection carries no machine-state tail');
  const serialized = JSON.stringify(ux);
  for (const term of [
    'ready_for_next_turn',
    'Verifying',
    'verifying',
    'machine state',
    'run state',
    'proof state',
    'daemon',
    'stream',
  ]) {
    assert.ok(!serialized.includes(term), `the chat projection must not carry ${term}`);
  }
});

test('attention is aggregated to at most three groups, with absence omitted', () => {
  const ux = projectUx({
    permissions: [{ id: 'p1' }, { id: 'p2' }],
    activeRunId: 'run-1',
    verification: { status: 'failed', checksFailed: 1 },
    task: {
      goal: 'ship',
      blockers: [
        'which module should own this?',
        'clarify the requirement',
        'the repository is read-only',
      ],
    },
  });
  assert.equal(ux.attention.length, 3);
  assert.deepEqual(
    ux.attention.map((item) => item.kind),
    ['Approval', 'FailedCheck', 'AmbiguousRequirement'],
  );
  assert.equal(ux.attention[0].count, 2);
  assert.deepEqual(aggregateAttention({}), []);
  assert.equal(aggregateAttention({}).length, 0);
});

test('unknown checks are "Checks to run · N", never Needs disclosure', () => {
  const ux = projectUx({
    task: {
      goal: 'ship',
      state: 'running',
      acceptanceCriteria: ['build', 'tests', 'lint'],
    },
  });
  assert.equal(ux.summary.checks, 'Checks to run · 3');
  assert.equal(ux.summary.wrong, null);
  assert.equal(aggregateAttention({ task: { acceptanceCriteria: ['a', 'b'] } }).length, 0);
});

test('the five-question summary omits absent rows and humanizes money', () => {
  const empty = projectUx({});
  assert.deepEqual(empty.summary, {
    doing: null,
    wrong: null,
    changed: null,
    checks: null,
    cost: null,
  });
  const ux = projectUx({
    task: {
      goal: 'ship it',
      state: 'running',
      acceptanceCriteria: [],
      budget: { spentCostMicro: '5000000' },
    },
    changes: { rows: [{ path: 'a.ts' }] },
  });
  assert.equal(ux.summary.doing, 'ship it');
  assert.equal(ux.summary.changed, '1 file changed');
  assert.equal(ux.summary.cost, '$5.00');
  assert.equal(humanUsd('5000000'), '$5.00');
  assert.equal(humanUsd('1'), 'under $0.01');
  assert.equal(humanUsd('0'), '$0.00');
});

test('refresh never reads billing/account; Ask reads conversation only', () => {
  for (const askOnly of [true, false]) {
    const plan = refreshReadPlan({ askOnly });
    assert.ok(!plan.includes('account'), 'run/chat refresh never reads account');
    assert.ok(!plan.includes('billing'), 'run/chat refresh never reads billing');
    assert.ok(!plan.includes('subscription'), 'run/chat refresh never reads subscription');
    assert.ok(!plan.includes('entitlements'), 'run/chat refresh never reads entitlements');
  }
  const ask = refreshReadPlan({ askOnly: true });
  assert.ok(ask.includes('conversation'));
  assert.ok(!ask.includes('tasks'));
  assert.ok(!ask.includes('taskRuns'));
  assert.ok(!ask.includes('proof'));
  assert.ok(!ask.includes('verification'));
  const task = refreshReadPlan({ askOnly: false });
  assert.ok(task.includes('tasks'));
  assert.ok(task.includes('taskRuns'));
  assert.ok(task.includes('proof'));
  assert.deepEqual(accountReadPlan(), ['account']);
});

test('Ask carries no task contract member', () => {
  assert.equal(UX_INVARIANTS.askCarriesContract, false);
  const ux = projectUx({}, 'ask');
  assert.equal(ux.intent, 'ask');
  assert.ok(!('completionContract' in ux));
  assert.ok(!('contractRows' in ux));
});

if (process.exitCode === 1) {
  console.error('FAILED');
} else {
  console.log(`\n${passed} UX state checks passed`);
}
