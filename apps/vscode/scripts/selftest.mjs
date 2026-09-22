#!/usr/bin/env node
// Faktor VS Code extension selftest. Plain `node scripts/selftest.mjs`, no
// npm install, no test framework. It imports the real TypeScript modules
// (Node >= 23.6 strips types natively) and drives them with a fake fetch /
// fake SSE stream:
//
//   1. nativeClient accept paths for every endpoint the extension uses,
//      including the v1 additive contract: unknown RESPONSE fields are
//      ignored while known fields keep exact-type validation;
//   2. nativeClient reject paths: hostile shapes, bad types, API error
//      envelopes and oversized bodies all fail loudly;
//   3. eventStream: cursor resume, backoff, heartbeat tolerance, replay
//      suppression, malformed-frame reporting and bounded frames;
//   4. the state store and transcript reducer (durable pages + SSE frames);
//   5. daemon binary resolution / refusal.
//
// Prints one line per check and exits nonzero on any failure.

import * as nc from '../src/nativeClient.ts';
import * as es from '../src/eventStream.ts';
import * as st from '../src/state.ts';
import * as dm from '../src/daemon.ts';
import * as ts from '../src/taskStart.ts';
import * as wb from '../src/workspaceBinding.ts';
import * as px from '../src/pixelAgents.ts';
import * as cp from '../src/cockpit.ts';
import * as cpa from '../src/controlPlaneAuth.ts';
import * as mn from '../src/money.ts';
import composerPolicy from '../media/composer-state.js';
import {
  chmodSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  writeFileSync,
} from 'node:fs';
import { spawn } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import vm from 'node:vm';
// `--packaged <extension-dir>` additionally asserts the extracted VSIX layout
// (out/ + media/, the Faktor-owned panel) without a daemon.
const packagedIndex = process.argv.indexOf('--packaged');
const packagedDir = packagedIndex !== -1 ? process.argv[packagedIndex + 1] : null;

// ------------------------------------------------------------- test harness

let passed = 0;
let failed = 0;

function pass(label, detail) {
  passed += 1;
  console.log(`PASS  ${label}${detail ? ` — ${detail}` : ''}`);
}

async function test(label, fn) {
  try {
    await fn();
    pass(label);
  } catch (error) {
    failed += 1;
    console.error(`FAIL  ${label} — ${error && error.message ? error.message : String(error)}`);
  }
}

function assert(condition, message) {
  if (!condition) {
    throw new Error(message || 'assertion failed');
  }
}

/** JSON text that renders bigint values (money) without throwing. */
function jsonText(value) {
  return JSON.stringify(value, (_key, entry) =>
    typeof entry === 'bigint' ? `${entry}n` : entry,
  );
}

function assertEqual(actual, expected, message) {
  if (actual !== expected) {
    throw new Error(
      `${message || 'assertEqual'}: expected ${jsonText(expected)}, got ${jsonText(actual)}`,
    );
  }
}

function assertDeepEqual(actual, expected, message) {
  const left = jsonText(actual);
  const right = jsonText(expected);
  if (left !== right) {
    throw new Error(`${message || 'assertDeepEqual'}: expected ${right}, got ${left}`);
  }
}

function assertProtocol(fn, needle) {
  try {
    fn();
  } catch (error) {
    assert(
      error instanceof nc.NativeProtocolError,
      `expected NativeProtocolError, got ${error && error.name}: ${error && error.message}`,
    );
    if (needle) {
      assert(
        error.message.includes(needle),
        `message ${JSON.stringify(error.message)} does not mention ${JSON.stringify(needle)}`,
      );
    }
    return;
  }
  throw new Error('expected a NativeProtocolError, nothing was thrown');
}

async function assertRejects(factory, predicate, label) {
  try {
    await factory();
  } catch (error) {
    if (predicate) {
      assert(
        predicate(error),
        `${label || 'rejection'}: predicate rejected ${error && error.name}: ${error && error.message}`,
      );
    }
    return;
  }
  throw new Error(`${label || 'rejection'}: expected the promise to reject`);
}

function clone(value) {
  // structuredClone keeps bigint money values exact (JSON.stringify cannot
  // serialize a BigInt); every fixture here is plain structured data.
  return structuredClone(value);
}

// ---------------------------------------------------------- payload builders

const healthJson = { ok: true, version: '9.9.9' };
const readyJson = { ready: true };
const sessionCreatedJson = { id: '7', title: 'selftest', created_ms: 1 };
const sessionSummaryJson = {
  id: '7',
  title: 'selftest',
  provider: 'fake',
  model: 'm',
  state: 'idle',
};
const modelInfoJson = {
  provider: 'fake',
  model: 'm',
  context: 8192,
  maxOutput: 2048,
  tools: true,
  parallelTools: false,
  reasoning: true,
  thinking: true,
  vision: false,
  structuredOutput: true,
  embeddings: false,
  streaming: true,
  source: 'providerCatalog',
};
const projectionJson = {
  session: { id: '7', title: 'selftest', provider: 'fake', model: 'm', lifecycle: 'open' },
  state: { machine: 'idle', label: 'Idle', active: false, terminal: false },
  activeModel: null,
  activeTool: null,
  progress: null,
  filesChanged: ['a.ts'],
  lastCheckpoint: null,
  verification: [{ opId: '1', tool: 'bash', startedMs: 2, effectStatus: 'unknown' }],
  contextUsage: null,
  queued: 0,
  prefixStability: null,
};
const budgetJson = {
  maxTokens: null,
  maxTurns: null,
  spentTokens: null,
  spentTurns: null,
  maxCostMicro: null,
  spentCostMicro: 12,
  openReservedMicro: 3,
};
const taskViewJson = {
  goal: 'ship it',
  constraints: [],
  state: 'running',
  milestones: { completed: [], open: ['main'] },
  decisions: [],
  failures: [],
  changedFiles: ['a.ts'],
  tests: { run: ['cargo test'], failed: ['cargo test'] },
  preferences: [],
  verification: [{ id: 'v1', detail: 'failed:cargo test', status: 'failed' }],
  progress: null,
  budget: budgetJson,
};
const checkpointJson = {
  sequence: 1,
  path: '/tmp/a.ts',
  beforeHash: null,
  afterHash: 'ab',
  beforeExists: false,
  afterExists: true,
  createdMs: 1,
  restoredMs: null,
};
const verificationViewJson = {
  owed: [
    { opId: '1', tool: 'bash', startedMs: 1, status: 'running', effectStatus: 'unknown' },
  ],
  failedChecks: [{ id: 'v1', detail: 'failed:cargo test', status: 'failed' }],
};
const taskRunJson = {
  task_id: 1,
  run_id: 'r1',
  mode: 'in_session',
  state: 'Running',
  goal: 'ship it',
  item_ids: ['main'],
  model: null,
};
const taskRunStartedJson = { task_id: 1, run_id: 'r1', state: 'Pending' };
const taskRunCancelledJson = { run_id: 'r1', cancelled: true };
const agentsJson = [
  {
    agent_id: 'r1',
    kind: 'self',
    run_id: 'r1',
    session_id: 7,
    worktree_id: 1,
    goal: 'ship it',
    state: 'Running',
    model: null,
    budget: null,
    ownership: 'self',
    capabilities: [],
    progress: null,
    result: null,
    item_ids: ['main'],
  },
  {
    agent_id: 'c1',
    kind: 'child',
    run_id: 'r1',
    session_id: 8,
    worktree_id: 2,
    goal: 'implement main',
    state: 'Running',
    model: 'm',
    provider: 'fake',
    budget: 1000,
    ownership: 'orchestrator',
    capabilities: ['ReadWorkspace'],
    progress: { phase: 'work' },
    result: null,
    item_id: 'main',
    item_kind: 'Implementation',
  },
];
const controlAckJson = { queuedSeq: 3, applied: null };
const presentationAckJson = { child_id: 'c1', presentation: 'background', changed: true };
const boardPostJson = {
  id: 3,
  board_id: 7,
  author_child: 8,
  author_session: 8,
  subject: 'handoff',
  body: 'main step ready',
  refs: ['evidence:41'],
  revision: 3,
  created_ms: 1700,
};
const boardRootPostJson = {
  id: 2,
  board_id: 7,
  author_child: null,
  author_session: 7,
  subject: 'root note',
  body: 'no children yet',
  refs: [],
  revision: 2,
  created_ms: 1600,
};
const boardPageJson = {
  board_id: 7,
  revision: 3,
  posts: [boardPostJson, boardRootPostJson],
  next_before_revision: 2,
  has_more: true,
};
const emptyBoardPageJson = {
  board_id: 7,
  revision: 0,
  posts: [],
  next_before_revision: null,
  has_more: false,
};
const tournamentJson = {
  id: 't-1',
  run_family: 'run-7',
  goal: 'pick winner',
  criteria: [{ id: 'c-1', spec: 'tests pass' }],
  candidates: [
    {
      child_id: 'child-0',
      worktree: '/tmp/w0',
      base_revision: 'abc',
      state: 'done',
      verification: 12,
      verification_pass: true,
      review: { rank: 'clean', reviewer: 'rev-1' },
      cost_micro: 100,
      wall_ms: 1000,
    },
    {
      child_id: 'child-1',
      worktree: '/tmp/w1',
      base_revision: 'abc',
      state: 'running',
      verification: null,
      verification_pass: null,
      review: null,
      cost_micro: 50,
      wall_ms: 900,
    },
  ],
  winner: null,
  state: 'open',
};
const tournamentStartedJson = {
  tournament_id: 't-1',
  run_id: 'run-7',
  candidates: ['child-0', 'child-1'],
  state: 'open',
  winner: null,
};
const tournamentSummariesJson = [
  { id: 't-1', state: 'open', candidate_count: 2, winner: null, decided_ms: null },
  { id: 't-0', state: 'decided', candidate_count: 2, winner: 'child-0', decided_ms: 123 },
];
const tournamentDecisionJson = {
  tournament_id: 't-1',
  winner: 'child-0',
  rationale: 'winner child-0 (verification=pass)',
  discarded: [{ child_id: 'child-1', reason: 'candidate ended failed' }],
};
const messagePageJson = {
  sessionId: '7',
  messages: [
    {
      seq: 2,
      id: 2,
      role: 'assistant',
      createdMs: 2,
      data: {},
      parts: [{ kind: 'text', createdMs: 2, data: { text: 'hi' } }],
    },
  ],
  hasMore: false,
  nextBefore: null,
};
const eventPageJson = {
  sessionId: '7',
  events: [
    { seq: 1, kind: 'SessionCreated', state: 'idle', opId: null, tsMs: 3, payload: null },
  ],
  hasMore: false,
  nextCursor: null,
};
const reservationsJson = {
  open: { count: 0, predictedMicro: 0 },
  settled: { count: 1, predictedMicro: 11, spentMicro: 12, providerReportedMicro: 12 },
  refunded: { count: 0, predictedMicro: 0 },
  uncertain: { count: 0, predictedMicro: 0 },
  routeDecisions: [],
  truncated: false,
};
const sessionUsageJson = {
  sessionId: '7',
  providerCalls: {
    tokens: 130,
    prefixObservations: [{ rowId: 1, promptTokens: 100, stability: null }],
  },
  prefixStability: null,
  tasks: [{ taskId: 't1', budget: budgetJson, reservations: reservationsJson }],
};
const usageTotalsJson = {
  sessions: 1,
  totals: { budget: 0, spent: 0 },
  perSession: [{ sessionId: '7', budget: 0, spent: 0 }],
  durable: {
    sessionsWithCalls: 1,
    providerCalls: {
      tokens: 130,
      prefixObservations: 1,
      prefixTokens: 100,
      prefixStabilityObservations: 0,
    },
    taskSpend: { settledCostMicro: 12 },
    reservations: reservationsJson,
    truncated: false,
  },
};
const creditBalanceJson = {
  granted_micro: 5_000_000,
  consumed_micro: 2_000_000,
  refunded_micro: 100_000,
  held_micro: 250_000,
  pending_consumes: 1,
};
const usageBucketsJson = {
  input_tokens: 1000,
  output_tokens: 500,
  cache_read_tokens: 200,
  cache_write_tokens: 50,
  reasoning_tokens: 25,
  provider_cost_micro: 900_000,
  managed_cost_micro: 700_000,
  byok_cost_micro: 200_000,
  events: 12,
  corrected_events: 1,
};
const billingUsageJson = {
  ok: true,
  organization: 'org-local',
  fold: {
    organization_id: 'org-local',
    totals: usageBucketsJson,
    per_task: [
      {
        task_id: 3,
        run_id: 'r1',
        totals: { ...usageBucketsJson, input_tokens: 400, managed_cost_micro: 700_000 },
      },
    ],
    next_cursor: null,
  },
  credits: creditBalanceJson,
  items: [{ cursor: '9', event: { unit: 'input_tokens', quantity: 10 } }],
  nextCursor: '9',
};
const entitlementsJson = {
  ok: true,
  entitlements: {
    organization_id: 'org-local',
    billing_account_id: 'acct-1',
    plan_id: 'pro',
    plan_found: true,
    subscription_status: 'active',
    subscription_expires_ms: 1_800_000_000_000,
    subscription_active: true,
    features: ['managed_providers', 'byok', 'credits'],
    limits: {
      max_tokens_per_period: 100_000,
      max_managed_spend_micro_per_period: 1_000_000,
      min_credit_balance_micro: 1000,
      max_active_tasks: 4,
      max_children_per_task: 3,
      max_provider_attempts_per_task: 5,
    },
    credits: creditBalanceJson,
    managed_spend_micro: 700_000,
    byok_spend_micro: 200_000,
    total_tokens: 1775,
    in_flight: [
      {
        id: 'txn-1',
        organization: 'org-local',
        kind: 'integration',
        reference: 'run-1',
        started_ms: 1_750_000_000_000,
        ended_ms: null,
      },
    ],
    now_ms: 1_750_000_000_000,
  },
};
const identityJson = {
  ok: true,
  identity: {
    subject_kind: 'user',
    subject_id: 'u-1',
    display_name: 'Admin',
    email: 'admin@example.com',
    organization: 'org-local',
    organization_name: 'Local Org',
    role: 'admin',
    effective_actions: ['billing_read', 'credits_grant'],
  },
};
const memberIdentityJson = {
  ...clone(identityJson),
  identity: {
    ...clone(identityJson.identity),
    subject_id: 'u-2',
    display_name: 'Member',
    role: 'member',
    effective_actions: ['billing_read'],
  },
};
const creditGrantJson = { ok: true, duplicate: false, credits: creditBalanceJson };
const verificationRecordJson = {
  recordId: 'rec1',
  revision: 'rev1',
  workspaceId: '1',
  worktreeId: '1',
  treeHash: null,
  criteria: [{ criterionKey: 'build', passed: true, evidence: null }],
  checks: [
    {
      check: 'cargo test',
      program: 'cargo',
      args: ['test'],
      category: 'test',
      required: true,
      status: 'passed',
      startedMs: 1,
      finishedMs: 2,
      exit: 0,
      summary: null,
    },
  ],
  changedFiles: [{ path: 'a.ts', digestHex: 'ab', size: 4 }],
  unrelatedChanges: [],
  reviewer: 'review-bot',
  candidateProof: {
    taskRevision: 'rev1',
    baseManifestHash: null,
    candidateManifestHash: null,
    sourceDiffEvidence: null,
    riskReportEvidence: null,
    accountingSnapshotDigest: null,
    runId: 'run-1',
    runBaseSnapshot: null,
    candidateSnapshot: 'cand1234',
    sourcesDigest: null,
    changedFilesDigest: null,
    publishedCommit: 'abcdef12',
    remotePrHead: 'refs/9',
  },
  verifiedSnapshot: 'ver1234',
  basedOnSnapshot: 'base1234',
  sourceCount: 2,
  landedSnapshot: 'land1234',
  status: 'passed',
  startedMs: 1,
  completedMs: 2,
};
const taskVerificationJson = {
  sessionId: '7',
  taskId: 't1',
  records: [verificationRecordJson],
};
const evidenceJson = {
  id: 41,
  kind: 'terminal',
  sessionId: 7,
  workspaceId: 1,
  taskId: null,
  sourceRevision: null,
  compressibility: null,
  backingCompleteness: null,
  backingRetained: true,
  backingLen: 3,
  allowRanges: true,
  allowSearch: true,
  maxBytes: 1024,
};
const evidenceRetrievalJson = {
  id: 41,
  selector: { selector: 'all' },
  bytesBase64: Buffer.from('ok\n').toString('base64'),
  byteLen: 3,
  truncatedByPolicy: false,
};
const semanticStatusJson = {
  configured: false,
  providerCount: 0,
  providers: [],
  fallback: { id: 'fallback', version: 1, capabilities: { operations: {} } },
  snapshotState: { providers: [], fallback: true },
};
const abortAckJson = { aborted: ['1'] };

// --------------------------------------------------------------- fake fetch

function jsonResponse(value, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

function makeClient(routes, options = {}) {
  const calls = [];
  const fetchImpl = async (url, init) => {
    const parsed = new URL(url);
    const key = `${init.method} ${parsed.pathname}`;
    calls.push({
      url,
      method: init.method,
      path: parsed.pathname,
      query: Object.fromEntries(parsed.searchParams.entries()),
      headers: init.headers,
      body: init.body === undefined ? undefined : JSON.parse(init.body),
    });
    const handler = routes[key];
    if (!handler) {
      throw new Error(`unexpected request ${key} (${url})`);
    }
    return handler(calls[calls.length - 1]);
  };
  const client = new nc.NativeClient({
    baseUrl: 'http://127.0.0.1:9',
    bearerToken: 'selftest-token',
    fetch: fetchImpl,
    timeoutMs: 500,
    ...options,
  });
  return { client, calls };
}

function findCall(calls, method, path) {
  const call = calls.find((entry) => entry.method === method && entry.path === path);
  assert(call, `no ${method} ${path} request was made (calls: ${calls.map((c) => `${c.method} ${c.path}`).join(', ')})`);
  return call;
}

function sseResponse(frames, status = 200) {
  const encoder = new TextEncoder();
  const stream = new ReadableStream({
    start(controller) {
      for (const frame of frames) {
        controller.enqueue(encoder.encode(frame));
      }
      controller.close();
    },
  });
  return new Response(stream, {
    status,
    headers: { 'content-type': 'text/event-stream' },
  });
}

function frame(event, id, data) {
  const head = event === null ? '' : `event: ${event}\n`;
  const idLine = id === null ? '' : `id: ${id}\n`;
  return `${head}${idLine}data: ${data}\n\n`;
}

// ----------------------------------------------------- 1. validator accepts

async function validatorAccepts() {
  await test('validators accept the documented shapes', () => {
    assertEqual(nc.validateHealth(clone(healthJson)).version, '9.9.9');
    assertEqual(nc.validateReady(clone(readyJson)).ready, true);
    assertEqual(nc.validateSessionCreated(clone(sessionCreatedJson)).id, '7');
    assertEqual(nc.validateSessionList({ sessions: [clone(sessionSummaryJson)] }).length, 1);
    assertEqual(nc.validateModelCatalog([clone(modelInfoJson)]).length, 1);
    assertEqual(nc.validateProjection(clone(projectionJson)).filesChanged[0], 'a.ts');
    assertEqual(nc.validateTurns([{ opId: '1', status: 'completed', provider: 'p', model: 'm', variant: null, toolMode: null, startedAt: 1, updatedMs: 2, queueSeq: null, promptMessageId: null }]).length, 1);
    assertEqual(nc.validateTaskViews([clone(taskViewJson)])[0].budget.spentCostMicro, 12n);
    assertEqual(nc.validateCheckpoints([clone(checkpointJson)])[0].sequence, 1);
    assertEqual(nc.validateVerificationView(clone(verificationViewJson)).owed.length, 1);
    assertEqual(nc.validateTaskRuns([clone(taskRunJson)])[0].run_id, 'r1');
    assertEqual(nc.validateTaskRun(clone(taskRunJson)).task_id, 1);
    assertEqual(nc.validateTaskRunStarted(clone(taskRunStartedJson)).state, 'Pending');
    assertEqual(
      nc.validateAttachmentId({
        digest: 'a'.repeat(64),
        mime: 'application/pdf',
        filename: null,
        size: 8,
      }).size,
      8,
    );
    assertEqual(nc.validateTaskRunCancelled(clone(taskRunCancelledJson)).cancelled, true);
    assertEqual(nc.validateAgents(clone(agentsJson)).length, 2);
    assertEqual(nc.validateAgents(clone(agentsJson))[1].presentation, 'foreground');
    const presented = clone(agentsJson);
    presented[1].presentation = 'background';
    assertEqual(nc.validateAgents(presented)[1].presentation, 'background');
    assertEqual(nc.validateAgentControlAck(clone(controlAckJson), 'test').queuedSeq, 3);
    assertEqual(nc.validateAgentPresentationAck(clone(presentationAckJson), 'test').presentation, 'background');
    assertEqual(nc.validateAgents(clone(agentsJson))[1].provider, 'fake');
    assertEqual(nc.validateAgents(clone(agentsJson))[0].provider, null, 'self entries carry no provider');
    assertEqual(nc.validateTournament(clone(tournamentJson)).candidates[0].reviewRank, 'clean');
    assertEqual(nc.validateTournament(clone(tournamentJson)).candidates[0].verificationPass, true);
    assertEqual(nc.validateTournament(clone(tournamentJson)).candidates[1].reviewRank, null);
    assertEqual(nc.validateTournamentStarted(clone(tournamentStartedJson)).candidates.length, 2);
    assertEqual(nc.validateTournamentSummaries(clone(tournamentSummariesJson)).length, 2);
    assertEqual(nc.validateTournamentDecision(clone(tournamentDecisionJson)).winner, 'child-0');
    // v1 additive: a pre-provider daemon entry validates with provider null.
    const legacyAgent = clone(agentsJson[1]);
    delete legacyAgent.provider;
    assertEqual(nc.validateAgents([legacyAgent])[0].provider, null);
    assertEqual(nc.validateMessagePage(clone(messagePageJson)).messages[0].parts[0].kind, 'text');
    assertEqual(nc.validateEventPage(clone(eventPageJson)).events[0].seq, 1);
    assertEqual(nc.validateSessionUsage(clone(sessionUsageJson)).tasks[0].taskId, 't1');
    assertEqual(nc.validateUsage(clone(usageTotalsJson)).durable.providerCalls.tokens, 130);
    const aggregateUsage = clone(usageTotalsJson);
    delete aggregateUsage.durable.reservations.routeDecisions;
    assertDeepEqual(nc.validateUsage(aggregateUsage).durable.reservations.routeDecisions, []);
    assertEqual(nc.validateTaskVerification(clone(taskVerificationJson)).records[0].checks[0].exit, 0);
    assertEqual(nc.validateBoardPage(clone(boardPageJson)).posts[0].revision, 3);
    assertEqual(nc.validateBoardPage(clone(boardPageJson)).posts[1].author_child, null);
    assertEqual(nc.validateBoardPage(clone(boardPageJson)).next_before_revision, 2);
    assertEqual(nc.validateBoardPage(clone(emptyBoardPageJson)).revision, 0, 'an empty board has revision 0');
    assertEqual(nc.validateBoardPost(clone(boardPostJson)).subject, 'handoff');
    assertEqual(nc.validateEvidence(clone(evidenceJson)).id, 41);
    assertEqual(nc.validateEvidenceRetrieval(clone(evidenceRetrievalJson)).byteLen, 3);
    assertEqual(nc.validateSemanticStatus(clone(semanticStatusJson)).providerCount, 0);
    assertEqual(nc.validateSemanticStatus(clone(semanticStatusJson)).fallback.version, 1);
    assertEqual(nc.validateAbortAck(clone(abortAckJson)).aborted[0], '1');
    const billingUsage = nc.validateBillingUsage(clone(billingUsageJson));
    assertEqual(billingUsage.fold.totals.managed_cost_micro, 700_000n);
    assertEqual(billingUsage.fold.per_task[0].task_id, 3);
    assertEqual(billingUsage.credits.held_micro, 250_000n);
    assertEqual(billingUsage.nextCursor, '9');
    const entitlements = nc.validateEntitlements(clone(entitlementsJson));
    assertEqual(entitlements.entitlements.plan_id, 'pro');
    assertEqual(entitlements.entitlements.limits.max_tokens_per_period, 100_000n);
    assertEqual(entitlements.entitlements.subscription_active, true);
    assertEqual(nc.validateIdentity(clone(identityJson)).identity.effective_actions.includes('credits_grant'), true);
    assertEqual(nc.validateCreditGrant(clone(creditGrantJson)).duplicate, false);
    // A pre-billing daemon serving no `email` on the identity is tolerated.
    const legacyIdentity = clone(identityJson);
    delete legacyIdentity.identity.email;
    assertEqual(nc.validateIdentity(legacyIdentity).identity.email, null);

    // v1 additive contract: a newer daemon's unknown optional field is
    // ignored at every nesting level, never a rejection.
    assertEqual(nc.validateHealth({ ...clone(healthJson), daemon_build: 'future' }).version, '9.9.9');
    assertEqual(nc.validateReady({ ...clone(readyJson), queue_depth: 0 }).ready, true);
    assertEqual(
      nc.validateProjection({
        ...clone(projectionJson),
        state: { ...projectionJson.state, futureLabel: 'x' },
        futureRoot: { nested: true },
      }).queued,
      0,
    );
    assertEqual(
      nc.validateTaskViews([{ ...clone(taskViewJson), futureTaskField: [1, 2] }])[0].goal,
      'ship it',
    );
  });
}

// ------------------------------------------------------ 2. validator rejects

async function validatorRejects() {
  await test('validators reject missing fields and bad known-field types', () => {
    assertProtocol(() => nc.validateHealth({ ok: true }), 'missing required field version');
    assertProtocol(() => nc.validateHealth({ ok: true, version: 7 }), 'expected a string, got number');
    assertProtocol(() => nc.validateReady({ ready: 'yes' }), 'expected a boolean');
    assertProtocol(
      () => nc.validateModelCatalog([{ ...clone(modelInfoJson), context: '8192' }]),
      'expected a finite number',
    );
    assertProtocol(
      () => nc.validateProjection({ ...clone(projectionJson), state: { machine: 'idle', label: 'Idle', active: false, terminal: false, rogue: 1 }, queued: '0' }),
      'expected a finite number',
    );
    assertProtocol(() => nc.validateAgents([{ ...clone(agentsJson[1]), kind: 'parent' }]), 'expected "self" or "child"');
    assertProtocol(() => nc.validateAgents([{ ...clone(agentsJson[1]), budget: 1.5 }]), 'expected an integer or null');
    assertProtocol(
      () => nc.validateAgents([{ ...clone(agentsJson[1]), presentation: 'hidden' }]),
      'expected "foreground" or "background"',
    );
    assertProtocol(
      () => nc.validateAgents([{ ...clone(agentsJson[1]), provider: 7 }]),
      'expected a string or null',
    );
    assertProtocol(
      () =>
        nc.validateTournament({
          ...clone(tournamentJson),
          candidates: [{ ...tournamentJson.candidates[0], cost_micro: 'not-a-number' }],
        }),
      'expected a decimal micro amount string',
    );
    assertProtocol(
      () =>
        nc.validateTournament({
          ...clone(tournamentJson),
          candidates: [
            { ...tournamentJson.candidates[0], cost_micro: Number.MAX_SAFE_INTEGER + 1 },
          ],
        }),
      'not exactly representable',
    );
    assertProtocol(
      () => nc.validateTournamentDecision({ tournament_id: 't', winner: 'c' }),
      'missing required field rationale',
    );
    assertProtocol(
      () => nc.validateAgentPresentationAck({ child_id: 'c1', presentation: null, changed: true }, 'test'),
      'expected "foreground" or "background"',
    );
    assertProtocol(() => nc.validateMessagePage({ ...clone(messagePageJson), messages: [{ seq: '2' }] }), 'missing required field id');
    assertProtocol(() => nc.validateEventPage({ ...clone(eventPageJson), events: [{ ...eventPageJson.events[0], opId: 7 }] }), 'expected a string or null');
    const missingDurable = clone(usageTotalsJson);
    delete missingDurable.durable;
    assertProtocol(() => nc.validateUsage(missingDurable), 'missing required field durable');
    assertProtocol(() => nc.validateSemanticStatus({ ...clone(semanticStatusJson), fallback: null }), 'expected an object');
    assertProtocol(() => nc.validateCheckpoints([{ ...clone(checkpointJson), beforeExists: 'nope' }]), 'expected a boolean');
    assertProtocol(
      () => nc.validateAttachmentId({ digest: 'ZZ', mime: 'application/pdf', filename: null, size: 8 }),
      '64-char lowercase hex digest',
    );
    assertProtocol(
      () => nc.validateAttachmentId({ digest: 'a'.repeat(64), mime: 'application/pdf', filename: null }),
      'missing required field size',
    );
    // Money is exact: the canonical string form is accepted (the legacy
    // number form stays accepted below the flag threshold, see moneyTests),
    // while a non-decimal string and an unsafe number both fail loudly.
    assertEqual(
      nc.validateTaskViews([
        { ...clone(taskViewJson), budget: { ...clone(budgetJson), spentCostMicro: '12' } },
      ])[0].budget.spentCostMicro,
      12n,
      'a decimal-string money value is exact',
    );
    assertProtocol(
      () =>
        nc.validateTaskViews([
          { ...clone(taskViewJson), budget: { ...clone(budgetJson), spentCostMicro: '12.5' } },
        ]),
      'expected a decimal micro amount string',
    );
    assertProtocol(
      () =>
        nc.validateTaskViews([
          {
            ...clone(taskViewJson),
            budget: { ...clone(budgetJson), spentCostMicro: Number.MAX_SAFE_INTEGER + 1 },
          },
        ]),
      'not exactly representable',
    );
    // Board: absent fields, hostile types and phantom entries fail loudly.
    const missingBoardField = clone(boardPageJson);
    delete missingBoardField.has_more;
    assertProtocol(() => nc.validateBoardPage(missingBoardField), 'missing required field has_more');
    assertProtocol(
      () => nc.validateBoardPage({ ...clone(boardPageJson), posts: [{ ...clone(boardPostJson), revision: 0 }] }),
      'expected a positive integer',
    );
    assertProtocol(
      () => nc.validateBoardPage({ ...clone(boardPageJson), posts: [{ ...clone(boardPostJson), author_child: 'root' }] }),
      'expected an integer or null',
    );
    assertProtocol(
      () => nc.validateBoardPage({ ...clone(boardPageJson), revision: -1 }),
      'expected a non-negative integer',
    );
    assertProtocol(
      () => nc.validateBoardPage({ ...clone(boardPageJson), posts: [{ ...clone(boardPostJson), refs: [7] }] }),
      'expected a string',
    );
    assertProtocol(
      () => nc.validateBoardPost({ ...clone(boardPostJson), id: 3.5 }),
      'expected an integer',
    );
    assertProtocol(() => nc.validateBoardPost({}), 'missing required field id');
    // Billing payloads: a missing known field, a typed mismatch and a
    // hostile limit value all fail loudly (never a silently wrong panel).
    const missingCredits = clone(billingUsageJson);
    delete missingCredits.credits;
    assertProtocol(() => nc.validateBillingUsage(missingCredits), 'missing required field credits');
    assertProtocol(
      () => nc.validateBillingUsage({ ...clone(billingUsageJson), nextCursor: 9 }),
      'expected a string or null',
    );
    assertEqual(
      nc.validateBillingUsage({
        ...clone(billingUsageJson),
        credits: { ...creditBalanceJson, held_micro: '1' },
      }).credits.held_micro,
      1n,
      'a decimal-string credit field is exact',
    );
    assertProtocol(
      () =>
        nc.validateBillingUsage({
          ...clone(billingUsageJson),
          credits: { ...creditBalanceJson, held_micro: '0x10' },
        }),
      'expected a decimal micro amount string',
    );
    assertProtocol(
      () =>
        nc.validateBillingUsage({
          ...clone(billingUsageJson),
          credits: { ...creditBalanceJson, held_micro: Number.MAX_SAFE_INTEGER + 1 },
        }),
      'not exactly representable',
    );
    assertProtocol(
      () => nc.validateEntitlements({ ...clone(entitlementsJson), entitlements: { ...entitlementsJson.entitlements, limits: { max_tokens_per_period: -1 } } }),
      'expected a non-negative limit',
    );
    assertProtocol(
      () => nc.validateEntitlements({ ...clone(entitlementsJson), entitlements: { ...entitlementsJson.entitlements, subscription_status: 7 } }),
      'expected a string or null',
    );
    assertProtocol(
      () => nc.validateIdentity({ ...clone(identityJson), identity: { ...identityJson.identity, effective_actions: 'credits_grant' } }),
      'expected an array',
    );
    assertProtocol(
      () => nc.validateCreditGrant({ ok: true, duplicate: false }),
      'missing required field credits',
    );
  });
}

// -------------------------------------------------------------- 3. client IO

async function clientAccepts() {
  await test('client speaks every native endpoint with strict validation', async () => {
    const routes = {
      'GET /native/health': () => jsonResponse(healthJson),
      'GET /native/ready': () => jsonResponse(readyJson),
      'POST /session/create': () => jsonResponse(sessionCreatedJson),
      'GET /session/list': () => jsonResponse({ sessions: [sessionSummaryJson] }),
      'GET /session/7/projection': () => jsonResponse(projectionJson),
      'GET /models': () => jsonResponse([modelInfoJson]),
      'GET /native/session/7/turns': () => jsonResponse([]),
      'GET /native/session/7/tasks': () => jsonResponse([taskViewJson]),
      'GET /native/session/7/checkpoints': () => jsonResponse([checkpointJson]),
      'GET /native/session/7/verification': () => jsonResponse(verificationViewJson),
      'GET /native/session/7/task-runs': () => jsonResponse([taskRunJson]),
      'GET /native/session/7/task-runs/r1': () => jsonResponse(taskRunJson),
      'POST /native/session/7/task-runs': () => jsonResponse(taskRunStartedJson),
      'POST /native/session/7/attachments': () =>
        jsonResponse({ digest: 'a'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 8 }),
      'POST /native/session/7/task-runs/r1/cancel': () => jsonResponse(taskRunCancelledJson),
      'GET /native/agents': () => jsonResponse(agentsJson),
      'POST /native/agents/c1/pause': () => jsonResponse(controlAckJson),
      'POST /native/agents/c1/resume': () => jsonResponse(controlAckJson),
      'POST /native/agents/c1/cancel': () => jsonResponse(controlAckJson),
      'POST /native/agents/c1/retry': () => jsonResponse(controlAckJson),
      'POST /native/agents/c1/steer': () => jsonResponse(controlAckJson),
      'POST /native/agents/c1/model': () => jsonResponse(controlAckJson),
      'POST /native/agents/c1/budget': () => jsonResponse(controlAckJson),
      'POST /native/session/7/agents/c1/presentation': () => jsonResponse(presentationAckJson),
      'GET /native/session/7/tournaments': () => jsonResponse(tournamentSummariesJson),
      'GET /native/session/7/tournament/t-1': () => jsonResponse(tournamentJson),
      'POST /native/session/7/tournament': () => jsonResponse(tournamentStartedJson),
      'POST /native/session/7/tournaments/t-1/decide': () => jsonResponse(tournamentDecisionJson),
      'POST /native/session/7/tournaments/t-1/abort': () =>
        jsonResponse({ ...clone(tournamentJson), state: 'aborted' }),
      'GET /native/session/7/board': () => jsonResponse(boardPageJson),
      'POST /native/session/7/board': () => jsonResponse(boardPostJson),
      'GET /native/messages': () => jsonResponse(messagePageJson),
      'GET /native/events': () => jsonResponse(eventPageJson),
      'GET /native/usage': (call) =>
        jsonResponse(call.query.org ? billingUsageJson : usageTotalsJson),
      'GET /native/session/7/usage': () => jsonResponse(sessionUsageJson),
      'GET /native/identity': () => jsonResponse(identityJson),
      'GET /native/entitlements': () => jsonResponse(entitlementsJson),
      'POST /native/credits/grant': () => jsonResponse(creditGrantJson),
      'GET /native/session/7/tasks/t1/verification': () => jsonResponse(taskVerificationJson),
      'GET /native/evidence/41': () => jsonResponse(evidenceJson),
      'POST /native/evidence/41/retrieve': () => jsonResponse(evidenceRetrievalJson),
      'GET /native/semantic/status': () => jsonResponse(semanticStatusJson),
      'POST /native/session/7/abort': () => jsonResponse(abortAckJson),
    };
    const { client, calls } = makeClient(routes);

    assertEqual((await client.health()).ok, true);
    assertEqual((await client.ready()).ready, true);
    assertEqual((await client.listSessions())[0].id, '7');
    assertEqual((await client.modelCatalog())[0].model, 'm');
    assertEqual(
      (await client.createSession({ provider: 'fake', model: 'm', workspace: '/w', title: 'selftest' })).id,
      '7',
    );
    assertEqual((await client.projection('7')).queued, 0);
    assertDeepEqual(await client.turns('7'), []);
    assertEqual((await client.tasks('7'))[0].goal, 'ship it');
    assertEqual((await client.checkpoints('7'))[0].path, '/tmp/a.ts');
    assertEqual((await client.verification('7')).failedChecks.length, 1);
    assertEqual((await client.taskRuns('7'))[0].state, 'Running');
    assertEqual((await client.taskRunState('7', 'r1')).run_id, 'r1');    assertEqual((await client.startTaskRun('7', { goal: 'ship it' })).run_id, 'r1');
    assertEqual((await client.uploadAttachment('7', { mime: 'application/pdf', filename: 'spec.pdf', data_base64: 'eA==' })).digest, 'a'.repeat(64));
    assertEqual((await client.cancelTaskRun('7', 'r1')).cancelled, true);
    assertEqual((await client.agents('7')).length, 2);
    assertEqual((await client.pauseAgent('c1')).queuedSeq, 3);
    assertEqual((await client.resumeAgent('c1')).queuedSeq, 3);
    assertEqual((await client.cancelAgent('c1')).queuedSeq, 3);
    assertEqual((await client.retryAgent('c1')).queuedSeq, 3);
    assertEqual((await client.steerAgent('c1', 'focus')).queuedSeq, 3);
    assertEqual((await client.setAgentModel('c1', 'm')).queuedSeq, 3);
    assertEqual((await client.setAgentBudget('c1', { max_tokens: 1000 })).queuedSeq, 3);
    assertEqual(
      (await client.setAgentPresentation('7', 'c1', 'background')).presentation,
      'background',
    );
    assertEqual((await client.tournaments('7'))[0].id, 't-1');
    assertEqual((await client.tournamentState('7', 't-1')).candidates.length, 2);
    assertEqual(
      (await client.startTournament('7', { goal: 'pick', criteria: ['tests pass'], n: 2 })).state,
      'open',
    );
    assertEqual((await client.decideTournament('7', 't-1')).winner, 'child-0');
    assertEqual((await client.abortTournament('7', 't-1', 'smoke reason')).state, 'aborted');
    assertEqual((await client.board('7', { since: 9, limit: 2 })).posts[0].subject, 'handoff');
    assertEqual((await client.board('7')).next_before_revision, 2);
    assertEqual(
      (await client.boardPost('7', { subject: 'status', body: 'all green', refs: ['evidence:41'] }))
        .revision,
      3,
    );
    assertEqual((await client.messages('7', { before: 9, limit: 2 })).messages[0].id, 2);
    assertEqual((await client.events('7', { after: 7, limit: 3 })).events[0].seq, 1);
    assertEqual((await client.usage()).sessions, 1);
    assertEqual((await client.sessionUsage('7')).providerCalls.tokens, 130);
    assertEqual((await client.identity()).identity.organization, 'org-local');
    assertEqual((await client.entitlements()).entitlements.plan_found, true);
    assertEqual(
      (await client.billingUsage('org-local', { since: '9', limit: 25 })).fold.totals.byok_cost_micro,
      200_000n,
    );
    assertEqual((await client.billingUsage('org-local')).nextCursor, '9');
    assertEqual(
      (await client.grantCredits({ amountMicro: 1_000_000, reason: 'top up', idempotencyKey: 'selftest-key-1' }))
        .duplicate,
      false,
    );
    assertEqual((await client.taskVerification('7', 't1')).records[0].recordId, 'rec1');
    assertEqual((await client.evidence('7', 41)).backingRetained, true);
    assertEqual((await client.retrieveEvidence('7', 41, { selector: 'all' })).byteLen, 3);
    assertEqual((await client.semanticStatus()).snapshotState.fallback, true);
    assertEqual((await client.abortSession('7', '3')).aborted[0], '1');

    // Request construction: auth, bodies, paths, cursor paging.
    const health = findCall(calls, 'GET', '/native/health');
    assertEqual(health.headers.Authorization, 'Bearer selftest-token');
    assertDeepEqual(findCall(calls, 'POST', '/session/create').body, {
      provider: 'fake',
      model: 'm',
      workspace: '/w',
      title: 'selftest',
    });
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/task-runs').body, { goal: 'ship it' });
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/attachments').body, {
      mime: 'application/pdf',
      filename: 'spec.pdf',
      data_base64: 'eA==',
    });
    assertDeepEqual(findCall(calls, 'GET', '/native/messages').query, {
      session: '7',
      before: '9',
      limit: '2',
    });
    assertDeepEqual(findCall(calls, 'GET', '/native/events').query, {
      session: '7',
      after: '7',
      limit: '3',
    });
    assertDeepEqual(findCall(calls, 'GET', '/native/agents').query, { session: '7' });
    assertDeepEqual(findCall(calls, 'POST', '/native/agents/c1/steer').body, { text: 'focus' });
    assertDeepEqual(findCall(calls, 'POST', '/native/agents/c1/model').body, { model: 'm' });
    assertDeepEqual(findCall(calls, 'POST', '/native/agents/c1/budget').body, { max_tokens: 1000 });
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/agents/c1/presentation').body, {
      state: 'background',
    });
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/tournament').body, {
      goal: 'pick',
      criteria: ['tests pass'],
      n: 2,
    });
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/tournaments/t-1/decide').body, {});
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/tournaments/t-1/abort').body, {
      reason: 'smoke reason',
    });
    assertDeepEqual(findCall(calls, 'POST', '/native/evidence/41/retrieve').body, {
      selector: 'all',
    });
    assertDeepEqual(findCall(calls, 'POST', '/native/evidence/41/retrieve').query, { session: '7' });
    assertDeepEqual(findCall(calls, 'GET', '/native/session/7/board').query, {
      since: '9',
      limit: '2',
    });
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/board').body, {
      subject: 'status',
      body: 'all green',
      refs: ['evidence:41'],
    });
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/abort').body, {
      session_id: '7',
      op_id: '3',
    });
    // Billing request construction: the org/since/limit query, the
    // idempotency header and the strict grant body (the legacy usage read
    // carries no org query, so the billing call is matched by its org).
    const billingCall = calls.find(
      (call) => call.method === 'GET' && call.path === '/native/usage' && call.query.org !== undefined,
    );
    assert(billingCall, 'the billing usage read must carry an org query');
    assertDeepEqual(billingCall.query, {
      org: 'org-local',
      since: '9',
      limit: '25',
    });
    assertEqual(
      findCall(calls, 'POST', '/native/credits/grant').headers['idempotency-key'],
      'selftest-key-1',
    );
    assertDeepEqual(findCall(calls, 'POST', '/native/credits/grant').body, {
      amount_micro: 1_000_000,
      reason: 'top up',
    });
  });

  await test('billing reads carry the control-plane token only when configured', async () => {
    const routes = {
      'GET /native/identity': () => jsonResponse(identityJson),
    };
    const withToken = makeClient(routes, { controlToken: 'cp-selftest-token' });
    assertEqual((await withToken.client.identity()).identity.role, 'admin');
    assertEqual(
      findCall(withToken.calls, 'GET', '/native/identity').headers['x-faktor-control-token'],
      'cp-selftest-token',
    );
    const withoutToken = makeClient(routes);
    await withoutToken.client.identity();
    assertEqual(
      findCall(withoutToken.calls, 'GET', '/native/identity').headers['x-faktor-control-token'],
      undefined,
      'an unconfigured control token is never sent',
    );
  });
}

async function clientRejects() {
  await test('client accepts additive response fields and rejects known-field type drift', async () => {
    const additive = makeClient({
      'GET /native/health': () => jsonResponse({ ok: true, version: '1', future_optional: { hint: 'v2' } }),
    });
    assertEqual((await additive.client.health()).version, '1');

    const drift = makeClient({
      'GET /native/health': () => jsonResponse({ ok: 'yes', version: '1' }),
    });
    await assertRejects(
      () => drift.client.health(),
      (error) => error instanceof nc.NativeProtocolError && /expected a boolean/.test(error.message),
      'known-field type drift',
    );

    const missing = makeClient({ 'GET /native/health': () => jsonResponse({ ok: true }) });
    await assertRejects(
      () => missing.client.health(),
      (error) => error instanceof nc.NativeProtocolError && /missing required field version/.test(error.message),
      'missing known field',
    );
  });

  await test('client maps API error envelopes and rejects malformed ones', async () => {
    const api = makeClient({
      'GET /native/health': () =>
        new Response(
          JSON.stringify({ error: { code: 'unauthorized', message: 'nope', retryable: false } }),
          { status: 401 },
        ),
    });
    await assertRejects(
      () => api.client.health(),
      (error) => error instanceof nc.NativeApiError && error.code === 'unauthorized' && error.status === 401,
      'api error envelope',
    );
    const garbage = makeClient({
      'GET /native/health': () => new Response('not json at all', { status: 500 }),
    });
    await assertRejects(
      () => garbage.client.health(),
      (error) => error instanceof nc.NativeApiError && error.code === 'http_error',
      'non-JSON error body',
    );
    const envelopeOnly = makeClient({
      'GET /native/health': () => new Response(JSON.stringify({ error: { code: 'x' } }), { status: 500 }),
    });
    await assertRejects(
      () => envelopeOnly.client.health(),
      (error) => error instanceof nc.NativeProtocolError,
      'error envelope without message',
    );
  });

  await test('client bounds response bodies and request time', async () => {
    const { client } = makeClient(
      { 'GET /native/health': () => jsonResponse({ ok: true, version: 'x'.repeat(4096) }) },
      { maxBodyBytes: 64 },
    );
    await assertRejects(
      () => client.health(),
      (error) => error instanceof nc.NativeProtocolError && /exceeded bound/.test(error.message),
      'streamed body bound',
    );

    const declared = {
      status: 200,
      ok: true,
      headers: { get: (name) => (name.toLowerCase() === 'content-length' ? '100000' : null) },
      body: null,
      arrayBuffer: async () => new ArrayBuffer(0),
      text: async () => '',
    };
    const declaredFetch = new nc.NativeClient({
      baseUrl: 'http://127.0.0.1:9',
      bearerToken: 't',
      fetch: async () => declared,
      maxBodyBytes: 32,
    });
    await assertRejects(
      () => declaredFetch.health(),
      (error) => error instanceof nc.NativeProtocolError && /declared body/.test(error.message),
      'declared body bound',
    );

    const hanging = new nc.NativeClient({
      baseUrl: 'http://127.0.0.1:9',
      bearerToken: 't',
      timeoutMs: 20,
      fetch: (url, init) =>
        new Promise((resolve, reject) => {
          init.signal.addEventListener('abort', () => reject(new Error('aborted')));
        }),
    });
    await assertRejects(
      () => hanging.health(),
      (error) => error instanceof nc.NativeProtocolError && /timed out/.test(error.message),
      'request timeout',
    );
  });
}

// ------------------------------------------------------------ 4. eventStream

async function eventStreamTests() {
  await test('eventStream tolerates heartbeats, suppresses replay, reports bad frames', async () => {
    const urls = [];
    const delivered = [];
    const errors = [];
    const statuses = [];
    let stream = null;
    const fetchImpl = async (url) => {
      urls.push(url);
      return sseResponse([
        frame('message_created', 1, '{"event":"message_created","session_id":"5","message":{"id":"1"}}'),
        frame('heartbeat', null, '{}'),
        ': keep-alive\n\n',
        frame('agent_state_changed', 2, '{"event":"agent_state_changed","session_id":"5","state":"streaming","label":"streaming"}'),
        frame('agent_state_changed', 2, '{"event":"agent_state_changed","session_id":"5","state":"streaming","label":"streaming"}'),
        frame('error', 3, 'not-json'),
        frame('error', 4, '{"event":"agent_state_changed","session_id":"5"}'),
        frame('heartbeat', 5, '{}'),
      ]);
    };
    stream = new es.EventStream({
      baseUrl: 'http://127.0.0.1:9/',
      bearerToken: 'tok',
      sessionId: '5',
      cursor: 0,
      fetch: fetchImpl,
      sleep: async () => {},
      onEvent: (event) => delivered.push(event),
      onStatus: (status, detail) => {
        statuses.push(status);
        if (status === 'retrying' && String(detail).startsWith('stream ended')) {
          stream.stop();
        }
      },
      onError: (error) => errors.push(error),
    });
    stream.start();
    await stream.whenStopped();
    assertDeepEqual(delivered.map((event) => event.id), [1, 2]);
    assertEqual(stream.cursor, 5, 'heartbeat ids must advance the cursor');
    assert(statuses.includes('open'), `statuses included open: ${statuses.join(',')}`);
    assertEqual(errors.length, 2);
    assert(
      errors.every((error) => error instanceof es.EventStreamProtocolError),
      `protocol errors expected: ${errors.map((e) => e.message).join(' | ')}`,
    );
    assertEqual(new URL(urls[0]).searchParams.get('events_after'), '0');
    assertEqual(new URL(urls[0]).searchParams.get('session'), null);
  });

  await test('eventStream resumes from the cursor with backoff', async () => {
    const urls = [];
    const delivered = [];
    const sleeps = [];
    let call = 0;
    let stream = null;
    const fetchImpl = async (url) => {
      urls.push(url);
      call += 1;
      if (call === 1) {
        return sseResponse([frame('heartbeat', 7, '{}')]);
      }
      return sseResponse([
        frame('agent_state_changed', 7, '{"event":"agent_state_changed","session_id":"5","state":"x","label":"x"}'),
        frame('agent_state_changed', 8, '{"event":"agent_state_changed","session_id":"5","state":"y","label":"y"}'),
      ]);
    };
    stream = new es.EventStream({
      baseUrl: 'http://127.0.0.1:9',
      bearerToken: 'tok',
      sessionId: '5',
      cursor: 6,
      fetch: fetchImpl,
      minBackoffMs: 5,
      maxBackoffMs: 50,
      jitter: () => 0,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
      onEvent: (event) => {
        delivered.push(event.id);
        if (event.id === 8) {
          stream.stop();
        }
      },
    });
    stream.start();
    await stream.whenStopped();
    assertEqual(urls.length, 2, 'one reconnect was expected');
    assertEqual(new URL(urls[0]).searchParams.get('events_after'), '6');
    assertEqual(new URL(urls[1]).searchParams.get('events_after'), '7');
    assertDeepEqual(delivered, [8], 'the replayed frame behind the cursor must not redeliver');
    assert(sleeps.length >= 1 && sleeps[0] >= 5, `backoff slept: ${JSON.stringify(sleeps)}`);
    assertEqual(stream.status, 'stopped');
  });

  await test('eventStream surfaces transport rejection and bounds frames', async () => {
    const errors = [];
    let stream = null;
    stream = new es.EventStream({
      baseUrl: 'http://127.0.0.1:9',
      bearerToken: 'tok',
      sessionId: '5',
      fetch: async () => new Response('denied', { status: 401 }),
      sleep: async () => {},
      onEvent: () => {},
      onError: (error) => {
        errors.push(error);
        stream.stop();
      },
    });
    stream.start();
    await stream.whenStopped();
    assertEqual(errors.length, 1);
    assert(errors[0].message.includes('HTTP 401'), errors[0].message);

    const frameErrors = [];
    let bounded = null;
    bounded = new es.EventStream({
      baseUrl: 'http://127.0.0.1:9',
      bearerToken: 'tok',
      sessionId: '5',
      maxFrameBytes: 32,
      fetch: async () => sseResponse(['data: ' + 'x'.repeat(256)]),
      sleep: async () => {},
      onEvent: () => {},
      onError: (error) => {
        frameErrors.push(error);
        bounded.stop();
      },
    });
    bounded.start();
    await bounded.whenStopped();
    assertEqual(frameErrors.length, 1);
    assert(/unterminated frame exceeded/.test(frameErrors[0].message), frameErrors[0].message);
  });
}

// ------------------------------------------------------------ 5. state store

async function stateTests() {
  await test('store notifies subscribers exactly once per change', () => {
    const store = new st.FaktorStore();
    let calls = 0;
    const unsubscribe = store.subscribe(() => {
      calls += 1;
    });
    store.patch({ daemon: 'starting' });
    assertEqual(calls, 1);
    store.patch({ daemon: 'starting' });
    assertEqual(calls, 1, 'same-value patches must not notify');
    store.patch({ daemon: 'running', daemonDetail: 'up' });
    assertEqual(calls, 2);
    assertEqual(store.snapshot().daemon, 'running');
    unsubscribe();
    store.patch({ daemon: 'stopped' });
    assertEqual(calls, 2, 'unsubscribed listeners must not fire');
  });

  await test('transcript reducer renders durable message pages and SSE frames', () => {
    const messages = [
      {
        seq: 2,
        id: 2,
        role: 'assistant',
        createdMs: 2,
        data: {},
        parts: [
          { kind: 'text', createdMs: 2, data: { text: 'hello' } },
          {
            kind: 'tool_call',
            createdMs: 2,
            data: { tool_call_id: 'c1', name: 'bash', input: { cmd: 'ls' }, state: 'running' },
          },
          {
            kind: 'tool_result',
            createdMs: 2,
            data: { tool_call_id: 'c1', excerpt: 'ok', exit_code: 0, artifact: 'evidence:41' },
          },
        ],
      },
      {
        seq: 1,
        id: 1,
        role: 'user',
        createdMs: 1,
        data: { files: [], text: 'do it' },
        parts: [],
      },
    ];
    const entries = st.transcriptFromMessages(messages);
    assertEqual(entries.length, 2);
    assertEqual(entries[0].role, 'user');
    assertEqual(entries[0].text, 'do it');
    assertEqual(entries[1].text, 'hello', 'durable text parts must render');
    assertEqual(entries[1].tools.length, 1);
    assertEqual(entries[1].tools[0].name, 'bash');
    assertEqual(entries[1].tools[0].state, 'running');
    assertEqual(entries[1].tools[0].excerpt, 'ok');
    assertEqual(entries[1].tools[0].exitCode, 0);
    assertEqual(entries[1].tools[0].artifact, 'evidence:41');

    let sse = st.applySseEvent([], 'message_created', {
      event: 'message_created',
      session_id: '7',
      message: {
        id: '9',
        role: 'assistant',
        seq: 9,
        created_ms: 1,
        parts: [{ type: 'text', text: 'hi' }],
      },
    });
    assertEqual(sse.length, 1);
    sse = st.applySseEvent(sse, 'message_part_updated', {
      message_id: '9',
      part: { type: 'text', text: ' there' },
    });
    assertEqual(sse[0].text, 'hi there');
    sse = st.applySseEvent(sse, 'message_part_updated', {
      message_id: '9',
      part: { type: 'tool_call', tool_call_id: 'c1', name: 'bash', input: {}, state: 'running' },
    });
    assertEqual(sse[0].tools[0].state, 'running');
    sse = st.applySseEvent(sse, 'tool_call_state', { tool_call_id: 'c1', state: 'completed' });
    assertEqual(sse[0].tools[0].state, 'completed');
    assert(st.applySseEvent(sse, 'unrelated_event', {}) === sse, 'unrelated events must not clone');
  });

  await test('transcript is bounded to MAX_TRANSCRIPT_ENTRIES', () => {
    const messages = [];
    for (let i = 0; i < st.MAX_TRANSCRIPT_ENTRIES + 25; i += 1) {
      messages.push({ seq: i, id: String(i), role: 'user', createdMs: i, data: {}, parts: [] });
    }
    const entries = st.transcriptFromMessages(messages);
    assertEqual(entries.length, st.MAX_TRANSCRIPT_ENTRIES);
    assertEqual(entries[entries.length - 1].seq, 0, 'newest messages survive the bound');
  });
}

// ---------------------------------------------------------------- 6. daemon

// A fake RELEASE process: writes its pid, serves the real /native/health
// contract with the bearer claim, and stays up until signalled.
const FAKE_RELEASE_SOURCE = `#!/usr/bin/env node
const http = require('node:http');
const fs = require('node:fs');
const pidfile = process.env.FAKE_RELEASE_PIDFILE;
const digest = 'a'.repeat(64);
const noDigest = process.env.FAKE_RELEASE_NO_DIGEST === '1';
const version = noDigest ? '9.9.9' : '9.9.9+release.selftest.' + digest;
const server = http.createServer((req, res) => {
  if (req.method === 'GET' && req.url === '/native/health' &&
      req.headers.authorization === 'Bearer ' + process.env.FAKTOR_SERVER_PASSWORD) {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end(JSON.stringify({ ok: true, version: version }));
    return;
  }
  res.writeHead(401, { 'content-type': 'application/json' });
  res.end('{}');
});
server.listen(0, '127.0.0.1', () => {
  if (pidfile) fs.writeFileSync(pidfile, String(process.pid));
  console.log('faktor server listening on http://127.0.0.1:' + server.address().port);
});
`;

// A fake bootstrap LAUNCHER: spawns the release, forwards its stdout, and
// either exits the moment readiness is seen (the new immediate-exit
// launcher, optionally announcing the release pid first) or stays resident
// (the legacy launcher). The announced pid/digest are overridable so tests
// can point the handshake at a victim pid or a mismatched digest.
const FAKE_LAUNCHER_SOURCE = `#!/usr/bin/env node
const { spawn } = require('node:child_process');
const path = require('node:path');
const resident = process.env.FAKE_LAUNCHER_MODE === 'resident';
const announce = process.env.FAKE_LAUNCHER_PIDLINE === '1';
const announcedPid = process.env.FAKE_LAUNCHER_ANNOUNCE_PID || null;
const announcedDigest = process.env.FAKE_LAUNCHER_ANNOUNCE_DIGEST || 'a'.repeat(64);
const release = path.join(__dirname, 'fake-release.cjs');
const child = spawn(process.execPath, [release], { stdio: ['ignore', 'pipe', 'inherit'] });
let buf = '';
let startupSeen = false;
child.stdout.on('data', (chunk) => {
  buf += chunk;
  let idx;
  while ((idx = buf.indexOf('\\n')) >= 0) {
    const line = buf.slice(0, idx);
    buf = buf.slice(idx + 1);
    if (!startupSeen && line.startsWith('faktor server listening on')) {
      startupSeen = true;
      if (announce) {
        console.log('faktor release started pid=' + (announcedPid || child.pid) + ' digest=' + announcedDigest);
      }
      console.log(line);
      if (!resident) {
        process.exit(0);
      }
      continue;
    }
    console.log(line);
  }
});
if (resident) {
  child.on('exit', (code) => process.exit(code === null ? 0 : code));
}
`;

function writeExecutable(path, source) {
  writeFileSync(path, source);
  chmodSync(path, 0o755);
}

function releasePidFrom(pidfile) {
  try {
    const pid = Number(readFileSync(pidfile, 'utf8').trim());
    return Number.isInteger(pid) && pid > 0 ? pid : null;
  } catch {
    return null;
  }
}

async function waitFor(predicate, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(`timed out waiting for ${label}`);
}

function fakeLauncherTree() {
  const root = mkdtempSync(join(tmpdir(), 'faktor-launcher-lifecycle-'));
  const release = join(root, 'fake-release.cjs');
  const launcher = join(root, 'fake-launcher.cjs');
  const pidfile = join(root, 'release.pid');
  writeExecutable(release, FAKE_RELEASE_SOURCE);
  writeExecutable(launcher, FAKE_LAUNCHER_SOURCE);
  return { root, launcher, pidfile };
}

async function daemonTests() {
  await test('daemon resolves an explicit binary path', () => {
    assertEqual(
      dm.findBinary({ workspaceRoot: '/nonexistent', binaryPath: '/tmp/fake-faktor-cli' }),
      '/tmp/fake-faktor-cli',
    );
  });

  await test('daemon refuses to start without a binary', async () => {
    const saved = process.env.FAKTOR_BIN;
    delete process.env.FAKTOR_BIN;
    try {
      await assertRejects(
        () =>
          dm.startDaemon({
            workspaceRoot: '/nonexistent-root-for-selftest',
            // Deterministic: an explicit install root with no layout can
            // never accidentally resolve through a real local install.
            installRoot: '/nonexistent-install-root-for-selftest',
          }),
        (error) => error instanceof Error && /binary not found/.test(error.message),
        'missing binary',
      );
    } finally {
      if (saved !== undefined) {
        process.env.FAKTOR_BIN = saved;
      }
    }
  });

  await test('daemon resolves through the bootstrap launcher when an install layout exists', () => {
    const root = mkdtempSync(join(tmpdir(), 'faktor-bootstrap-selftest-'));
    try {
      // No layout yet: legacy resolution (null bootstrap).
      assertEqual(dm.bootstrapBinary({ workspaceRoot: '/nonexistent', installRoot: root }), null);
      assertEqual(dm.installRootFor({ workspaceRoot: '/nonexistent', dataDir: '/tmp/data' }), join('/tmp/data', 'install'));
      assertEqual(
        dm.installRootFor({ workspaceRoot: '/nonexistent', installRoot: root, dataDir: '/tmp/data' }),
        root,
      );
      mkdirSync(join(root, 'versions'));
      writeFileSync(join(root, 'launcher'), '#!/bin/sh\n');
      assertEqual(dm.bootstrapBinary({ workspaceRoot: '/nonexistent', installRoot: root }), join(root, 'launcher'));
      // The explicit operator override still wins over the bootstrap.
      assertEqual(
        dm.resolveExecutable({
          workspaceRoot: '/nonexistent',
          installRoot: root,
          binaryPath: '/tmp/explicit-faktor-cli',
        }),
        '/tmp/explicit-faktor-cli',
      );
      // Without an override the bootstrap is what gets spawned.
      assertEqual(
        dm.resolveExecutable({ workspaceRoot: '/nonexistent', installRoot: root }),
        join(root, 'launcher'),
      );
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  await test('health release digests parse from the version and absence stays null', () => {
    const digest = 'a'.repeat(64);
    assertEqual(dm.releaseDigestOf(`0.9.1+release.0.9.1-abcdef123456.${digest}`), digest);
    assertEqual(dm.releaseDigestOf('0.9.1'), null);
    assertEqual(dm.releaseDigestOf('0.9.1+release.0.9.1-abcdef123456'), null);
    assertEqual(dm.releaseDigestOf(`0.9.1+release.x.${'A'.repeat(64)}`), null);
  });

  await test('launcher-exits-early-still-alive: the release pid is tracked, launcher exit is not daemon death', async () => {
    const { root, launcher, pidfile } = fakeLauncherTree();
    try {
      const daemon = await dm.startDaemon({
        workspaceRoot: '/nonexistent',
        binaryPath: launcher,
        resolveReleasePid: () => releasePidFrom(pidfile),
        env: { FAKE_RELEASE_PIDFILE: pidfile },
      });
      const releasePid = releasePidFrom(pidfile);
      assert(releasePid !== null, 'the fake release must have written its pid');
      assertEqual(daemon.pid, releasePid, 'the handle must track the RELEASE pid, never the launcher');
      assert(
        daemon.launcherPid !== releasePid,
        `launcher pid ${daemon.launcherPid} must differ from the release pid ${releasePid}`,
      );
      await waitFor(
        () => !dm.processAlive(daemon.launcherPid),
        5_000,
        'the immediate-exit launcher to disappear',
      );
      assertEqual(await daemon.alive(), true, 'the launcher exiting must not read as daemon death');
      assert(dm.processAlive(releasePid), 'the release must still be running');
      dm.stopDaemon(daemon);
      assertEqual(await daemon.alive(), false, 'a stopped daemon must report not alive');
      await waitFor(() => !dm.processAlive(releasePid), 5_000, 'the release to exit after stop');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  await test('launcher-exits-early-daemon-dies-detected: release death is reported once the launcher is gone', async () => {
    const { root, launcher, pidfile } = fakeLauncherTree();
    try {
      const daemon = await dm.startDaemon({
        workspaceRoot: '/nonexistent',
        binaryPath: launcher,
        resolveReleasePid: () => releasePidFrom(pidfile),
        env: { FAKE_RELEASE_PIDFILE: pidfile },
      });
      const releasePid = releasePidFrom(pidfile);
      assert(releasePid !== null, 'the fake release must have written its pid');
      await waitFor(
        () => !dm.processAlive(daemon.launcherPid),
        5_000,
        'the immediate-exit launcher to disappear',
      );
      process.kill(releasePid, 'SIGKILL');
      await waitFor(() => !dm.processAlive(releasePid), 5_000, 'the release to die');
      assertEqual(await daemon.alive(), false, 'the RELEASE death must be detected');
      dm.stopDaemon(daemon);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  await test('stop-terminates-release-not-launcher: stop kills the release, not just a resident launcher', async () => {
    const { root, launcher, pidfile } = fakeLauncherTree();
    try {
      const daemon = await dm.startDaemon({
        workspaceRoot: '/nonexistent',
        binaryPath: launcher,
        resolveReleasePid: () => releasePidFrom(pidfile),
        env: { FAKE_RELEASE_PIDFILE: pidfile, FAKE_LAUNCHER_MODE: 'resident' },
      });
      const releasePid = releasePidFrom(pidfile);
      assert(releasePid !== null, 'the fake release must have written its pid');
      assert(dm.processAlive(daemon.launcherPid), 'the legacy launcher must stay resident');
      assert(
        releasePid !== daemon.launcherPid,
        'the release must be a distinct process from the resident launcher',
      );
      dm.stopDaemon(daemon);
      await waitFor(() => !dm.processAlive(releasePid), 5_000, 'the RELEASE to die on stop');
      await waitFor(
        () => !dm.processAlive(daemon.launcherPid),
        5_000,
        'the resident launcher to exit after its release died',
      );
      assertEqual(await daemon.alive(), false);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  await test('launcher release-pid handshake line is parsed before the startup line', async () => {
    const digest = 'a'.repeat(64);
    const match = dm.RELEASE_PID_LINE.exec(`faktor release started pid=4242 digest=${digest}`);
    assert(match !== null, 'the frozen handshake line must parse');
    assertEqual(Number(match[1]), 4242);
    for (const hostile of [
      '',
      `faktor release started pid=4242 digest=${'A'.repeat(64)}`,
      `faktor release started pid=4242 digest=${'a'.repeat(63)}`,
      `faktor release started pid=0 digest=${digest}`,
      `leading noise faktor release started pid=4242 digest=${digest}`,
      `faktor release started pid=4242 digest=${digest} trailing`,
    ]) {
      assertEqual(dm.RELEASE_PID_LINE.exec(hostile), null, `hostile handshake ${JSON.stringify(hostile)}`);
    }

    const { root, launcher, pidfile } = fakeLauncherTree();
    try {
      const daemon = await dm.startDaemon({
        workspaceRoot: '/nonexistent',
        binaryPath: launcher,
        // No resolver injection: only the announced handshake pid can name
        // the release here (the test seam is deliberately absent).
        env: { FAKE_RELEASE_PIDFILE: pidfile, FAKE_LAUNCHER_PIDLINE: '1' },
      });
      const releasePid = releasePidFrom(pidfile);
      assert(releasePid !== null, 'the fake release must have written its pid');
      assertEqual(daemon.pid, releasePid, 'the announced release pid must be tracked');
      assertEqual(daemon.releaseTrustError, null, 'a matching health digest must be trusted');
      assert(
        daemon.pid !== daemon.launcherPid,
        'the announced release pid must not be the launcher pid',
      );
      assertEqual(await daemon.alive(), true);
      await dm.stopDaemon(daemon);
      await waitFor(() => !dm.processAlive(releasePid), 5_000, 'the release to exit after stop');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  await test('release handshake digest mismatch: announced pid is refused, never signalled', async () => {
    const { root, launcher, pidfile } = fakeLauncherTree();
    const victim = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {
      stdio: 'ignore',
    });
    try {
      const daemon = await dm.startDaemon({
        workspaceRoot: '/nonexistent',
        binaryPath: launcher,
        env: {
          FAKE_RELEASE_PIDFILE: pidfile,
          FAKE_LAUNCHER_PIDLINE: '1',
          FAKE_LAUNCHER_ANNOUNCE_PID: String(victim.pid),
          FAKE_LAUNCHER_ANNOUNCE_DIGEST: 'b'.repeat(64),
        },
      });
      const releasePid = releasePidFrom(pidfile);
      assert(releasePid !== null, 'the fake release must have written its pid');
      assertEqual(
        daemon.releaseTrustError?.code,
        'release_digest_mismatch',
        'a mismatched handshake digest must be refused typed',
      );
      assert(daemon.pid !== victim.pid, 'the refused announced pid must never be adopted');
      await waitFor(
        () => !dm.processAlive(daemon.launcherPid),
        5_000,
        'the immediate-exit launcher to disappear',
      );
      await dm.stopDaemon(daemon);
      assert(dm.processAlive(victim.pid), 'the refused announced pid must never be signalled');
      assertEqual(
        daemon.stopRefusal?.code,
        'release_digest_mismatch',
        'stop must report the typed refusal instead of a silent no-op',
      );
      assert(dm.processAlive(releasePid), 'the untrusted release must not be killed by this handle');
      process.kill(releasePid, 'SIGKILL');
    } finally {
      victim.kill('SIGKILL');
      rmSync(root, { recursive: true, force: true });
    }
  });

  await test('release handshake with no health digest is refused, never adopted', async () => {
    const { root, launcher, pidfile } = fakeLauncherTree();
    try {
      const daemon = await dm.startDaemon({
        workspaceRoot: '/nonexistent',
        binaryPath: launcher,
        env: {
          FAKE_RELEASE_PIDFILE: pidfile,
          FAKE_RELEASE_NO_DIGEST: '1',
          FAKE_LAUNCHER_PIDLINE: '1',
        },
      });
      const releasePid = releasePidFrom(pidfile);
      assert(releasePid !== null, 'the fake release must have written its pid');
      assertEqual(
        daemon.releaseTrustError?.code,
        'release_digest_absent',
        'an absent health digest must refuse the announcement',
      );
      assert(daemon.pid !== releasePid, 'the unverifiable announced pid must not be adopted');
      await waitFor(
        () => !dm.processAlive(daemon.launcherPid),
        5_000,
        'the immediate-exit launcher to disappear',
      );
      await dm.stopDaemon(daemon);
      assert(dm.processAlive(releasePid), 'the unverified release must not be signalled');
      assertEqual(daemon.stopRefusal?.code, 'release_digest_absent');
      process.kill(releasePid, 'SIGKILL');
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  await test('handshake pid disagreeing with the port probe is refused; the probed release wins', async () => {
    const { root, launcher, pidfile } = fakeLauncherTree();
    const victim = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {
      stdio: 'ignore',
    });
    try {
      const daemon = await dm.startDaemon({
        workspaceRoot: '/nonexistent',
        binaryPath: launcher,
        resolveReleasePid: () => releasePidFrom(pidfile),
        env: {
          FAKE_RELEASE_PIDFILE: pidfile,
          FAKE_LAUNCHER_PIDLINE: '1',
          FAKE_LAUNCHER_ANNOUNCE_PID: String(victim.pid),
        },
      });
      const releasePid = releasePidFrom(pidfile);
      assert(releasePid !== null, 'the fake release must have written its pid');
      assertEqual(
        daemon.releaseTrustError?.code,
        'release_pid_probe_mismatch',
        'a probe disagreement must refuse the announcement',
      );
      assertEqual(daemon.pid, releasePid, 'the probed, health-verified pid is the release');
      await dm.stopDaemon(daemon);
      await waitFor(() => !dm.processAlive(releasePid), 5_000, 'the probed release to be stopped');
      assert(dm.processAlive(victim.pid), 'the announced victim pid must never be signalled');
    } finally {
      victim.kill('SIGKILL');
      rmSync(root, { recursive: true, force: true });
    }
  });

  await test('recycled resolved pid: a dead daemon refuses to signal the cached pid', async () => {
    const { root, launcher, pidfile } = fakeLauncherTree();
    const victim = spawn(process.execPath, ['-e', 'setInterval(() => {}, 1000)'], {
      stdio: 'ignore',
    });
    try {
      const daemon = await dm.startDaemon({
        workspaceRoot: '/nonexistent',
        binaryPath: launcher,
        // Simulates a stale resolution that now names an unrelated process:
        // the pid is alive but owns no daemon identity.
        resolveReleasePid: () => victim.pid,
        env: { FAKE_RELEASE_PIDFILE: pidfile, FAKE_LAUNCHER_MODE: 'resident' },
      });
      const releasePid = releasePidFrom(pidfile);
      assert(releasePid !== null, 'the fake release must have written its pid');
      assertEqual(daemon.pid, victim.pid, 'the fixture resolves the stale pid');
      process.kill(releasePid, 'SIGKILL');
      await waitFor(() => !dm.processAlive(releasePid), 5_000, 'the release to die');
      const healthDeadline = Date.now() + 5_000;
      while (Date.now() < healthDeadline && (await daemon.alive())) {
        await new Promise((resolve) => setTimeout(resolve, 25));
      }
      assertEqual(await daemon.alive(), false, 'health must stop answering');
      await dm.stopDaemon(daemon);
      assert(dm.processAlive(victim.pid), 'a pid without health identity must never be signalled');
      assertEqual(
        daemon.stopRefusal?.code,
        'stop_identity_unverified',
        'the refusal must be typed',
      );
    } finally {
      victim.kill('SIGKILL');
      rmSync(root, { recursive: true, force: true });
    }
  });
}

// ---------------------------------------- 7. VS Code product defect fixes

async function shadowDefaultTests() {
  await test('P0 shadow-only: the setting vocabulary is shadow/empty and the removed mode never forwards', () => {
    const base = ts.startTaskRequest('goal', { mutationMode: '', maxTokens: 0, maxCostMicro: 0n });
    assert(!('mutation_mode' in base), `empty setting must omit mutation_mode: ${JSON.stringify(base)}`);
    assertDeepEqual(
      ts.startTaskRequest('goal', { mutationMode: 'shadow', maxTokens: 10, maxCostMicro: 5n }),
      { goal: 'goal', max_tokens: 10, max_cost_micro: 5, mutation_mode: 'shadow' },
    );
    // The removed direct-owner mode can never reach the wire, even from a
    // hostile/legacy caller that bypasses the setting enum.
    assert(
      !(
        'mutation_mode' in
        ts.startTaskRequest('goal', { mutationMode: 'direct_compat', maxTokens: 0, maxCostMicro: 0n })
      ),
      'the removed mode must never be forwarded',
    );
    const manifest = JSON.parse(
      readFileSync(new URL('../package.json', import.meta.url), 'utf8'),
    );
    const setting = manifest.contributes.configuration.properties['faktor.mutationMode'];
    assertEqual(setting.default, '', 'the setting default must be inherit-daemon');
    assertDeepEqual(setting.enum, ['shadow', ''], 'the setting must offer shadow/empty only');
    assert(!setting.enum.includes('direct_compat'), 'the removed mode must not be selectable');
  });

  await test('removed direct_compat is a typed strict-parse refusal before any request', async () => {
    const calls = [];
    const failures = [];
    const outcome = await ts.startTaskRun({
      client: {
        startTaskRun: async (sessionId, request) => {
          calls.push({ sessionId, request });
          return taskRunStartedJson;
        },
      },
      sessionId: '7',
      goal: 'ship it',
      settings: { mutationMode: 'direct_compat', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {
        throw new Error('must not start');
      },
      onFailure: (failure) => failures.push(failure),
    });
    assertEqual(calls.length, 0, 'a removed mode must never reach the daemon');
    assertEqual(outcome.ok, false);
    assertEqual(outcome.runId, null);
    assertEqual(failures.length, 1);
    assertEqual(failures[0].kind, 'validation');
    assertEqual(failures[0].status, null);
    assert(
      failures[0].message.includes('direct_compat') && failures[0].message.includes('removed'),
      failures[0].message,
    );
    // The strict parser is the one vocabulary authority: never coerces.
    assertDeepEqual(ts.parseMutationModeSetting(''), { mode: '' });
    assertDeepEqual(ts.parseMutationModeSetting('shadow'), { mode: 'shadow' });
    assert('reason' in ts.parseMutationModeSetting('direct_compat'));
    assert('reason' in ts.parseMutationModeSetting('nonsense'));
  });

  await test('409 shadow refusal is typed/actionable and attempted exactly once (no downgrade)', async () => {
    const calls = [];
    const conflict = new nc.NativeApiError(
      409,
      'conflict',
      'session 7 has no registered worktree row; shadowed mutating runs need a real owner worktree',
      false,
    );
    const client = {
      startTaskRun: async (sessionId, request) => {
        calls.push({ sessionId, request });
        throw conflict;
      },
    };
    const failures = [];
    const outcome = await ts.startTaskRun({
      client,
      sessionId: '7',
      goal: 'ship it',
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {
        throw new Error('must not start');
      },
      onFailure: (failure) => failures.push(failure),
    });
    assertEqual(calls.length, 1, 'a 409 must never be retried (no silent downgrade)');
    assert(!('mutation_mode' in calls[0].request), 'the refused request must carry no fabricated mode');
    assertEqual(outcome.ok, false);
    assertEqual(failures.length, 1);
    assertEqual(failures[0].kind, 'shadow_unregistered');
    assert(
      !failures[0].message.includes('direct_compat'),
      `message must not name a removed opt-in: ${failures[0].message}`,
    );
    assert(
      failures[0].message.includes('shadow') && failures[0].message.includes('registered'),
      `message must name the shadow/worktree cause: ${failures[0].message}`,
    );
    assert(failures[0].message.includes('native API error 409'), failures[0].message);
  });

  await test('start failures classify 4xx/5xx/transport and never start; success acks once', async () => {
    const classify = async (error) => {
      const client = {
        startTaskRun: async () => {
          throw error;
        },
      };
      const failures = [];
      const outcome = await ts.startTaskRun({
        client,
        sessionId: '7',
        goal: 'g',
        settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
        onStarted: () => {},
        onFailure: (failure) => failures.push(failure),
      });
      assertEqual(outcome.ok, false);
      assertEqual(failures.length, 1);
      return failures[0];
    };
    assertEqual((await classify(new nc.NativeApiError(400, 'malformed', 'bad body', false))).kind, 'validation');
    assertEqual((await classify(new nc.NativeApiError(500, 'internal', 'boom', false))).kind, 'server');
    assertEqual((await classify(new nc.NativeApiError(401, 'unauthorized', 'nope', false))).kind, 'auth');
    assertEqual((await classify(new Error('socket closed'))).kind, 'transport');

    const started = [];
    const outcome = await ts.startTaskRun({
      client: { startTaskRun: async () => taskRunStartedJson },
      sessionId: '7',
      goal: 'g',
      settings: { mutationMode: 'shadow', maxTokens: 0, maxCostMicro: 0n },
      onStarted: (run) => started.push(run),
      onFailure: () => {
        throw new Error('must not fail');
      },
    });
    assertEqual(outcome.ok, true);
    assertEqual(outcome.runId, 'r1');
    assertEqual(started.length, 1);
  });
}

// ------------------------- 7b. Task-mode completion contract + file forwarding

async function completionContractTests() {
  await test('completion contract parsing is strict; all-false is the default path', () => {
    assertDeepEqual(ts.completionContractSetting(undefined), null);
    assertDeepEqual(ts.completionContractSetting(null), null);
    assertDeepEqual(
      ts.completionContractSetting({
        include_commit: false,
        include_push: false,
        include_pr: false,
      }),
      null,
      'all-false is the default behavior and must not reach the wire',
    );
    assertDeepEqual(
      ts.completionContractSetting({
        include_commit: true,
        include_push: false,
        include_pr: true,
      }),
      { include_commit: true, include_push: false, include_pr: true },
    );
    // Hostile shapes are refused to null, never coerced or partially applied.
    for (const hostile of [
      'commit',
      ['include_commit'],
      true,
      42,
      {},
      { include_commit: true },
      { include_commit: 'yes', include_push: false, include_pr: false },
      { include_commit: 1, include_push: 0, include_pr: 0 },
      { include_commit: true, include_push: false, include_pr: false, include_release: true },
      Object.assign(
        Object.create({ include_commit: true, include_push: false, include_pr: false }),
        { include_pr: true, include_commit: true },
      ),
    ]) {
      assertDeepEqual(
        ts.completionContractSetting(hostile),
        null,
        `hostile contract must be refused: ${JSON.stringify(hostile)}`,
      );
    }
    // Inherited-only members cannot smuggle a contract through the parser.
    const inherited = Object.create({
      include_commit: true,
      include_push: false,
      include_pr: false,
    });
    assertDeepEqual(ts.completionContractSetting(inherited), null);
  });

  await test('a non-default contract starts an explicit work item; the default stays byte-identical', () => {
    const base = ts.startTaskRequest('goal', { mutationMode: '', maxTokens: 0, maxCostMicro: 0n });
    assertDeepEqual(base, { goal: 'goal' });
    assert(
      !('work_items' in base) && !('completion_contract' in base) && !('files' in base),
      'the default path carries no contract seam and no files',
    );
    assertDeepEqual(
      ts.startTaskRequest('goal', {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        files: ['src/a.ts', 'docs/b.md'],
      }),
      { goal: 'goal', files: ['src/a.ts', 'docs/b.md'] },
      'files-only task keeps the plain-prompt shape',
    );
    const requested = ts.startTaskRequest('goal', {
      mutationMode: 'shadow',
      maxTokens: 10,
      maxCostMicro: 5n,
      files: ['src/a.ts'],
      completionContract: { include_commit: true, include_push: false, include_pr: true },
    });
    assertDeepEqual(requested, {
      goal: 'goal',
      max_tokens: 10,
      max_cost_micro: 5,
      files: ['src/a.ts'],
      work_items: [
        {
          id: 'main',
          kind: 'Implementation',
          summary: 'goal',
          ownership: 'isolated_worktree',
        },
      ],
      completion_contract: { include_commit: true, include_push: false, include_pr: true },
      mutation_mode: 'shadow',
    });
    assertDeepEqual(
      ts.startTaskRequest('goal', {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        completionContract: { include_commit: false, include_push: false, include_pr: false },
      }),
      { goal: 'goal' },
      'an all-false contract is still the default path',
    );
  });

  await test('the task view parses the additive durable completion block and rejects malformed shapes', () => {
    const absent = nc.validateTaskViews([clone(taskViewJson)])[0];
    assertEqual(absent.completion, null, 'absent completion stays null, never fabricated');
    const served = nc.validateTaskViews([
      {
        ...clone(taskViewJson),
        completion: {
          contract: { include_commit: true, include_push: true, include_pr: false },
          steps: [
            { step: 'commit', status: 'succeeded', detail: 'committed abc', seq: 7, at_ms: 11 },
            { step: 'push', status: 'skipped', detail: 'no remote', seq: 8 },
          ],
        },
      },
    ])[0];
    assertEqual(served.completion.contract.include_push, true);
    assertEqual(served.completion.steps.length, 2);
    assertEqual(served.completion.steps[0].status, 'succeeded');
    assertEqual(served.completion.steps[0].detail, 'committed abc');
    assertEqual(served.completion.steps[0].atMs, 11);
    assertEqual(served.completion.steps[1].atMs, null);
    for (const hostile of [
      { completion: { contract: { include_commit: true, include_push: false }, steps: [] } },
      {
        completion: {
          contract: { include_commit: 'yes', include_push: false, include_pr: false },
          steps: [],
        },
      },
      {
        completion: {
          contract: { include_commit: true, include_push: false, include_pr: false },
          steps: 'none',
        },
      },
      {
        completion: {
          contract: { include_commit: true, include_push: false, include_pr: false },
          steps: [{ step: 'commit', status: 'failed' }],
        },
      },
    ]) {
      assertProtocol(() => nc.validateTaskViews([{ ...clone(taskViewJson), ...hostile }]));
    }
  });

  await test('cockpit renders the completion contract with explicit provenance', () => {
    const task = {
      goal: 'ship it',
      state: 'running',
      completed: [],
      open: ['main'],
      testsRun: [],
      testsFailed: [],
      changedFiles: [],
      budget: null,
      acceptanceCriteria: [],
      plan: [],
      blockers: [],
      evidenceRefs: [],
      phase: null,
      progress: null,
      completion: {
        includeCommit: true,
        includePush: false,
        includePr: true,
        steps: [
          { step: 'commit', status: 'pending', detail: 'awaiting the gate' },
          { step: 'pr', status: 'pending', detail: null },
        ],
        source: 'derived',
        reason: null,
      },
    };
    const view = cp.buildCockpit({
      task,
      agents: [],
      verification: null,
      usage: null,
      taskVerification: null,
    });
    const section = cp.cockpitSections(view).find((entry) => entry.key === 'completion');
    assert(section && section.present, 'the completion section must be present');
    assert(section.lines[0].includes('commit, pr'), JSON.stringify(section.lines));
    assert(section.lines[1].includes('[pending] commit'), JSON.stringify(section.lines));
    assert(section.lines[2].includes('[pending] pr'), JSON.stringify(section.lines));
    assert(
      section.lines.some((line) => line.includes('status source: derived')),
      JSON.stringify(section.lines),
    );
    // No contract: the section is explicitly empty, never fabricated.
    const bare = cp.buildCockpit({
      task: { ...task, completion: null },
      agents: [],
      verification: null,
      usage: null,
      taskVerification: null,
    });
    const empty = cp.cockpitSections(bare).find((entry) => entry.key === 'completion');
    assert(empty && empty.present === false && empty.lines[0].includes('none'));
  });

  await test('the built-in Task composer posts the checked contract and displays its statuses', () => {
    const contractTask = {
      goal: 'ship it',
      state: 'running',
      completed: [],
      open: [],
      testsRun: [],
      testsFailed: [],
      changedFiles: [],
      budget: null,
      acceptanceCriteria: [],
      plan: [],
      blockers: [],
      evidenceRefs: [],
      phase: null,
      progress: null,
      completion: {
        includeCommit: true,
        includePush: false,
        includePr: false,
        steps: [{ step: 'commit', status: 'unknown', detail: 'run ended' }],
        source: 'unavailable',
        reason: 'no native completion read',
      },
    };
    const snapshot = { ...webviewSnapshot([]), task: contractTask };
    const { posted, dom, deliver } = runChatWebview(snapshot);
    // Plain start: no contract field at all (chat never carries one).
    dom.document.getElementById('goal').value = 'plain goal';
    dom.document.getElementById('composer').dispatch('submit', { preventDefault() {} });
    assertDeepEqual(posted[posted.length - 1], { type: 'sendGoal', goal: 'plain goal' });
    // Checked boxes: the exact strict contract rides this task start only.
    dom.document.getElementById('goal').value = 'contracted goal';
    dom.document.getElementById('contract-commit').checked = true;
    dom.document.getElementById('contract-pr').checked = true;
    dom.document.getElementById('composer').dispatch('submit', { preventDefault() {} });
    assertDeepEqual(posted[posted.length - 1], {
      type: 'sendGoal',
      goal: 'contracted goal',
      completionContract: { include_commit: true, include_push: false, include_pr: true },
    });
    // A successful start resets the controls (per-start contract).
    deliver({
      type: 'startResult',
      goal: 'contracted goal',
      ok: true,
    });
    assert(
      dom.document.getElementById('contract-commit').checked === false &&
        dom.document.getElementById('contract-pr').checked === false,
      'a success ack clears the completion controls',
    );
    // The task card renders the durable status + its explicit source/reason.
    const completionNode = dom.document.getElementById('task-completion');
    assert(
      findFake(completionNode, (node) => String(node.textContent).includes('[unknown] commit')),
      JSON.stringify(completionNode.children.map((child) => child.textContent)),
    );
    assert(
      findFake(completionNode, (node) => String(node.textContent).includes('status source: unavailable')),
    );
    assert(
      findFake(completionNode, (node) => String(node.textContent).includes('no native completion read')),
    );
  });
}


// ------------------- 7b-2. pending submission envelope + admission restore

function pendingEnvelope(overrides = {}) {
  return {
    text: 'ship the screenshot',
    sessionId: '7',
    draftId: 'draft-1',
    messageId: 'msg-1',
    files: [{ url: 'data:image/png;base64,QUJD', mime: 'image/png', filename: 'shot.png' }],
    attachments: [],
    ...overrides,
  };
}

function binaryAttachment(overrides = {}) {
  return {
    mime: 'application/pdf',
    filename: 'spec.pdf',
    bytes: 3,
    dataBase64: Buffer.from('%PDF').toString('base64'),
    isImage: false,
    ...overrides,
  };
}

async function pendingSubmissionTests() {
  await test('typed attachments ride the task start beside workspace paths', () => {
    const id = { digest: 'a'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 4 };
    assertDeepEqual(
      ts.startTaskRequest('goal', {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        files: ['src/a.ts'],
        attachments: [id],
      }),
      { goal: 'goal', files: ['src/a.ts'], attachments: [id] },
    );
    assertDeepEqual(
      ts.startTaskRequest('goal', { mutationMode: '', maxTokens: 0, maxCostMicro: 0n }),
      { goal: 'goal' },
      'the attachment-free path stays byte-identical',
    );
  });

  await test('admission uploads bytes first and starts with the durable ids', async () => {
    const calls = [];
    const restores = [];
    const started = [];
    const client = {
      uploadAttachment: async (sessionId, request) => {
        calls.push({ kind: 'upload', sessionId, request });
        return { digest: 'b'.repeat(64), mime: request.mime, filename: request.filename ?? null, size: 3 };
      },
      startTaskRun: async (sessionId, request) => {
        calls.push({ kind: 'start', sessionId, request });
        return taskRunStartedJson;
      },
    };
    const outcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({ attachments: [binaryAttachment()] }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: (run) => started.push(run),
      onFailure: () => {},
      restore: (failure) => restores.push(failure),
    });
    assertEqual(outcome.ok, true);
    assertEqual(restores.length, 0, 'success never restores');
    assertEqual(calls.length, 2, 'one upload then exactly one start');
    assertEqual(calls[0].kind, 'upload');
    assertDeepEqual(calls[0].request, {
      mime: 'application/pdf',
      filename: 'spec.pdf',
      data_base64: Buffer.from('%PDF').toString('base64'),
    });
    assertEqual(calls[1].kind, 'start');
    assertDeepEqual(calls[1].request.attachments, [
      { digest: 'b'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 3 },
    ]);
    assertEqual(started.length, 1);
  });

  await test('every start failure restores the pending identity and never leaves a partial admission', async () => {
    const failures = [
      ['validation', new nc.NativeApiError(400, 'malformed', 'bad body', false)],
      ['unavailable model', new nc.NativeApiError(400, 'unknown_model', 'model "x" is not available', false)],
      ['existing run', new nc.NativeApiError(409, 'conflict', 'session already has a live run', false)],
      ['daemon loss', new Error('socket closed')],
    ];
    for (const [label, error] of failures) {
      const calls = [];
      const restores = [];
      const client = {
        uploadAttachment: async () => {
          calls.push('upload');
          return { digest: 'c'.repeat(64), mime: 'application/pdf', filename: null, size: 3 };
        },
        startTaskRun: async (sessionId, request) => {
          calls.push('start');
          throw error;
        },
      };
      const envelope = pendingEnvelope({ attachments: [binaryAttachment()] });
      const snapshot = JSON.stringify(envelope);
      const outcome = await ts.admitPendingSubmission({
        client,
        sessionId: '7',
        pending: envelope,
        settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
        onStarted: () => {
          throw new Error(`${label}: must not start`);
        },
        onFailure: () => {},
        restore: (failure) => restores.push(failure),
      });
      assertEqual(outcome.ok, false, label);
      assertEqual(outcome.runId, null, `${label}: no durable run id may leak`);
      assertDeepEqual(calls, ['upload', 'start'], `${label}: exactly one attempt each, no retry`);
      assertEqual(restores.length, 1, `${label}: restore exactly once`);
      // The ORIGINAL envelope identity/files survive verbatim, so the host
      // can restore the exact text and attachments into the draft.
      assertEqual(JSON.stringify(envelope), snapshot, `${label}: envelope untouched`);
      assert(envelope.text.length > 0, `${label}: restorable draft text must survive`);
      assertEqual(envelope.sessionId, '7');
      assertEqual(envelope.draftId, 'draft-1');
      assertEqual(envelope.messageId, 'msg-1');
      assertEqual(envelope.files[0].mime, 'image/png');
    }
  });

  await test('an upload failure restores the draft and never issues a task start', async () => {
    const calls = [];
    const restores = [];
    const client = {
      uploadAttachment: async () => {
        calls.push('upload');
        throw new nc.NativeApiError(413, 'oversized', 'attachment exceeds the bound', false);
      },
      startTaskRun: async () => {
        calls.push('start');
        throw new Error('must not start');
      },
    };
    const outcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({ attachments: [binaryAttachment()] }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: (failure) => restores.push(failure),
    });
    assertEqual(outcome.ok, false);
    assertDeepEqual(calls, ['upload'], 'no start after an upload failure');
    assertEqual(restores.length, 1);
    assertEqual(restores[0].kind, 'upload');
    assert(restores[0].message.includes('413') || restores[0].message.includes('upload'), restores[0].message);
  });

  await test('image submission is refused loudly and the draft/images remain', async () => {
    const calls = [];
    const restores = [];
    const client = {
      uploadAttachment: async () => {
        calls.push('upload');
        throw new Error('must not upload an image');
      },
      startTaskRun: async () => {
        calls.push('start');
        throw new Error('must not start an image submission');
      },
    };
    const image = binaryAttachment({
      mime: 'image/png',
      filename: 'shot.png',
      isImage: true,
      bytes: 4,
      dataBase64: Buffer.from([137, 80, 78, 71]).toString('base64'),
    });
    const envelope = pendingEnvelope({ attachments: [image] });
    const outcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: envelope,
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: (failure) => restores.push(failure),
    });
    assertEqual(outcome.ok, false);
    assertDeepEqual(calls, [], 'images are refused before any upload/start request');
    assertEqual(restores.length, 1);
    assertEqual(restores[0].kind, 'image_unsupported');
    assert(
      restores[0].message.includes('provider media/content parts are not wired'),
      restores[0].message,
    );
    assert(envelope.files.length > 0, 'the image draft payload is kept for the restore');
    assertEqual(outcome.attachmentIds.length, 0);
  });

  await test('pending envelopes are re-validated strictly at the host boundary', () => {
    const valid = ts.parsePendingSubmission(pendingEnvelope());
    assert(valid !== null && valid.draftId === 'draft-1' && valid.messageId === 'msg-1');
    assertEqual(ts.parsePendingSubmission(pendingEnvelope()).files.length, 1);
    for (const hostile of [
      null,
      'nope',
      [],
      { ...pendingEnvelope(), text: '   ' },
      { ...pendingEnvelope(), messageId: 'x'.repeat(4097) },
      { ...pendingEnvelope(), files: 'not-an-array' },
      { ...pendingEnvelope(), attachments: 'not-an-array' },
      { ...pendingEnvelope(), attachments: [{ mime: 'application/pdf' }] },
      {
        ...pendingEnvelope(),
        attachments: [{ ...binaryAttachment(), dataBase64: 'x'.repeat(10 * 1024 * 1024) }],
      },
      { ...pendingEnvelope(), attachments: [{ ...binaryAttachment(), bytes: -1 }] },
    ]) {
      assertEqual(ts.parsePendingSubmission(hostile), null, JSON.stringify(hostile).slice(0, 120));
    }
  });

  await test('a host-validated envelope with binary attachments uploads bytes first and starts with the durable ids', async () => {
    const pending = ts.parsePendingSubmission({
      text: 'attach spec',
      sessionId: '7',
      messageId: 'msg-9',
      draftId: 'draft-9',
      files: [],
      attachments: [
        {
          mime: 'application/pdf',
          filename: 'spec.pdf',
          bytes: 8,
          dataBase64: 'JVBERi0xLjQ=',
          isImage: false,
        },
      ],
    });
    assert(pending !== null, 'the host must accept the envelope');
    assertEqual(pending.attachments.length, 1, 'binary refs ride the pending envelope');
    assertEqual(pending.attachments[0].mime, 'application/pdf');
    assertEqual(pending.attachments[0].dataBase64, 'JVBERi0xLjQ=');
    assertEqual(pending.attachments[0].isImage, false);

    const calls2 = [];
    let startedRequest = null;
    const outcome = await ts.admitPendingSubmission({
      client: {
        uploadAttachment: async (sessionId, request) => {
          calls2.push({ kind: 'upload', request });
          return { digest: 'd'.repeat(64), mime: request.mime, filename: request.filename ?? null, size: 8 };
        },
        startTaskRun: async (sessionId, request) => {
          calls2.push({ kind: 'start' });
          startedRequest = request;
          return taskRunStartedJson;
        },
      },
      sessionId: '7',
      pending,
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {
        throw new Error('must not restore a successful admission');
      },
    });
    assertEqual(outcome.ok, true);
    assertDeepEqual(calls2[0].request, {
      mime: 'application/pdf',
      filename: 'spec.pdf',
      data_base64: 'JVBERi0xLjQ=',
    });
    assertDeepEqual(startedRequest.attachments, [
      { digest: 'd'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 8 },
    ]);
  });
}

// -------------- 7c. board state projection + webview forwarding hardening

async function boardAndForwardingTests() {
  await test('board pages project posts, unread and the read watermark', () => {
    const first = st.boardStateFromPage(clone(boardPageJson), null);
    assertEqual(first.board.available, true);
    assertEqual(first.board.source, 'native');
    assertEqual(first.board.revision, 3);
    assertEqual(first.board.unread, 2, 'a null watermark counts every page post as unread');
    assertEqual(first.board.posts[0].id, '3');
    assertEqual(first.board.posts[0].author, 'child:8');
    assertEqual(first.board.posts[1].author, 'root');
    assertDeepEqual(first.board.posts[0].refs, ['evidence:41']);
    assertEqual(first.board.posts[0].createdMs, 1700);
    assertEqual(first.watermark, 3);

    // The watermark acknowledges exactly the revisions at-or-below it.
    assertEqual(st.boardStateFromPage(clone(boardPageJson), 2).board.unread, 1);
    assertEqual(st.boardStateFromPage(clone(boardPageJson), 3).board.unread, 0);
    assertEqual(st.boardStateFromPage(clone(emptyBoardPageJson), 3).board.unread, 0);
    assertEqual(st.boardStateFromPage(clone(emptyBoardPageJson), 3).board.revision, 0);

    // Unavailable is explicit and never fabricates posts.
    const unavailable = st.unavailableBoardState('route absent (HTTP 404 not_found)');
    assertEqual(unavailable.available, false);
    assertEqual(unavailable.posts.length, 0);
    assertEqual(unavailable.unread, null);
    assert(unavailable.reason.includes('404'), unavailable.reason);

    // Host re-validation: board gestures are bounded before the wire.
    assertDeepEqual(st.parseBoardReadRequest(undefined, undefined), { since: null, limit: null });
    assertDeepEqual(st.parseBoardReadRequest(5, 10), { since: 5, limit: 10 });
    for (const [since, limit] of [
      [0, null],
      [-1, null],
      [1.5, null],
      ['5', null],
      [null, 0],
      [null, 101],
      [null, 1.5],
    ]) {
      const parsed = st.parseBoardReadRequest(since, limit);
      assert('reason' in parsed, `hostile board read must be refused: ${since}/${limit}`);
    }
    assertDeepEqual(st.parseBoardPostRequest({ subject: ' s ', body: ' b ', refs: [' r '] }), {
      subject: 's',
      body: ' b ',
      refs: ['r'],
    });
    for (const hostile of [
      {},
      { subject: '   ', body: 'b' },
      { subject: 's' },
      { subject: 's', body: '' },
      { subject: 's', body: 'b', refs: 'nope' },
      { subject: 's', body: 'b', refs: [''] },
      { subject: 's'.repeat(513), body: 'b' },
      { subject: 's', body: 'b'.repeat(16 * 1024 + 1) },
    ]) {
      const parsed = st.parseBoardPostRequest(hostile);
      assert('reason' in parsed, `hostile board post must be refused: ${JSON.stringify(hostile)}`);
    }
  });

  await test('the client refuses empty board posts locally (no request is made)', async () => {
    const { client, calls } = makeClient({});
    assertProtocol(() => client.boardPost('7', { subject: ' ', body: 'b' }));
    assertProtocol(() => client.boardPost('7', { subject: 's', body: ' ' }));
    assertEqual(calls.length, 0, 'invalid board posts must never reach the wire');
  });

  await test('composer files are bounded per-entry and forwarded to the native task-run', async () => {
    const { files, refused } = ts.boundedWebviewFiles([
      'src/a.ts',
      '  docs/b.md  ',
      '',
      '  ',
      7,
      null,
      '../escape.txt',
      'dir/../up.txt',
      'ctrl\u0000name',
      '/abs/escape.rs',
      'C:\\abs\\escape.rs',
      'data:image/png;base64,AAAA',
      'file:///etc/passwd',
      'x'.repeat(4097),
      ...Array.from({ length: 70 }, (_, i) => `f${i}.ts`),
    ]);
    assertEqual(files.length, 64, 'the accepted list is capped at MAX_WEBVIEW_FILES');
    assertEqual(files[0], 'src/a.ts');
    assertEqual(files[1], 'docs/b.md', 'accepted paths are trimmed');
    assert(
      refused.some((entry) => entry.reason.includes('traverses outside the workspace')),
      JSON.stringify(refused),
    );
    assert(
      refused.some((entry) => entry.reason.includes('control characters')),
      JSON.stringify(refused),
    );
    assert(
      refused.some((entry) => entry.reason.includes('4096')),
      JSON.stringify(refused),
    );
    assert(
      refused.some((entry) => entry.reason.includes('more than 64')),
      JSON.stringify(refused),
    );
    assert(
      refused.some((entry) => entry.reason.includes('workspace-relative paths only')),
      JSON.stringify(refused),
    );
    assert(
      refused.some((entry) => entry.reason.includes('must be workspace-relative')),
      JSON.stringify(refused),
    );
    assertDeepEqual(ts.boundedWebviewFiles(undefined), { files: [], refused: [] });
    assertEqual(ts.boundedWebviewFiles('nope').refused[0].index, 0);

    // The accepted subset reaches ONE native task-run request alongside the
    // submitted contract (an explicit work item), never a silent drop.
    const captured = [];
    const outcome = await ts.startTaskRun({
      client: {
        startTaskRun: async (_sessionId, request) => {
          captured.push(request);
          return taskRunStartedJson;
        },
      },
      sessionId: '7',
      goal: 'ship it',
      settings: {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        files,
        completionContract: { include_commit: true, include_push: false, include_pr: true },
      },
      onStarted: () => {},
      onFailure: () => {
        throw new Error('must not fail');
      },
    });
    assertEqual(outcome.ok, true);
    assertEqual(captured.length, 1);
    assertEqual(captured[0].files.length, 64);
    assertDeepEqual(captured[0].completion_contract, {
      include_commit: true,
      include_push: false,
      include_pr: true,
    });
    assertEqual(captured[0].work_items[0].id, 'main');
    assertEqual(captured[0].work_items[0].kind, 'Implementation');
  });

  await test('a malformed completion contract is refused with a typed reason', () => {
    assertDeepEqual(ts.parseCompletionContract(undefined), { contract: null });
    assertDeepEqual(ts.parseCompletionContract(null), { contract: null });
    assertDeepEqual(
      ts.parseCompletionContract({
        include_commit: false,
        include_push: false,
        include_pr: false,
      }),
      { contract: null },
      'all-false is the default path, not a contract',
    );
    assertDeepEqual(
      ts.parseCompletionContract({
        include_commit: true,
        include_push: false,
        include_pr: true,
      }),
      { contract: { include_commit: true, include_push: false, include_pr: true } },
    );
    for (const hostile of [
      'commit',
      ['include_commit'],
      true,
      42,
      {},
      { include_commit: true },
      { include_commit: 'yes', include_push: false, include_pr: false },
      { include_commit: true, include_push: false, include_pr: false, include_release: true },
    ]) {
      const parsed = ts.parseCompletionContract(hostile);
      assert(
        'reason' in parsed && parsed.reason.length > 0,
        `hostile contract must carry a typed reason: ${JSON.stringify(hostile)}`,
      );
    }
    // Inherited members cannot smuggle a contract through the parser.
    const inherited = Object.create({
      include_commit: true,
      include_push: false,
      include_pr: false,
    });
    const parsed = ts.parseCompletionContract(inherited);
    assert('reason' in parsed, 'inherited-only members must be refused');
  });
}

async function draftPreservationTests() {
  await test('composer clears the draft only after a successful start', () => {
    assertEqual(composerPolicy.afterStart('my draft', 'my draft', false), 'my draft');
    assertEqual(composerPolicy.afterStart('my draft', 'my draft', true), '');
    assertEqual(composerPolicy.afterStart('newer text', 'submitted', true), 'newer text');
    assertEqual(composerPolicy.keepDraft('  spaced  ', 'spaced', true), false);
    assertEqual(composerPolicy.keepDraft('spaced', 'spaced', false), true);
  });
}

async function runStateTests() {
  const make = (state, runId = 'r1') => ({
    taskId: '1',
    runId,
    mode: 'in_session',
    state,
    goal: null,
    model: null,
  });

  await test('busy/activeRunId derive from run STATE (terminal => not busy)', () => {
    assertEqual(st.isTerminalRunState('Done'), true);
    assertEqual(st.isTerminalRunState('Failed'), true);
    assertEqual(st.isTerminalRunState('Cancelled'), true);
    assertEqual(st.isTerminalRunState('Running'), false);
    assertEqual(st.activeRunIdAfter('r1', [make('Running')]), 'r1');
    assertEqual(st.activeRunIdAfter('r1', [make('Done')]), null, 'a listed Done run is not busy');
    assertEqual(st.activeRunIdAfter('r1', [make('Failed')]), null);
    assertEqual(st.activeRunIdAfter('r1', [make('Cancelled')]), null);
    assertEqual(st.activeRunIdAfter('r1', []), null, 'a vanished run is not busy');
    assertEqual(st.activeRunIdAfter(null, [make('Running')]), null);
  });

  await test('cancel targets only active runs; terminal runs are never attempted', () => {
    assertEqual(st.cancelRunTarget('r1', [make('Done')]), null, 'terminal tracked run => no cancel attempt');
    assertEqual(st.cancelRunTarget('r1', [make('Cancelled')]), null);
    assertEqual(st.cancelRunTarget('r1', [make('Running')]), 'r1');
    assertEqual(st.cancelRunTarget(null, [make('Done'), make('Running', 'r2')]), 'r2');
    assertEqual(st.cancelRunTarget(null, [make('Done'), make('Failed')]), null, 'all-terminal => no cancel attempt');
    assertEqual(st.cancelRunTarget('stale', [make('Running', 'r2')]), 'r2');
  });

  await test('a server 409 on cancel surfaces as a typed NativeApiError', async () => {
    const { client } = makeClient({
      'POST /native/session/7/task-runs/r1/cancel': () =>
        new Response(
          JSON.stringify({ error: { code: 'conflict', message: 'terminal runs refuse cancel', retryable: false } }),
          { status: 409 },
        ),
    });
    await assertRejects(
      () => client.cancelTaskRun('7', 'r1'),
      (error) => error instanceof nc.NativeApiError && error.status === 409 && error.code === 'conflict',
      'typed terminal-cancel refusal',
    );
  });
}

async function workspaceBindingTests() {
  await test('session binding uses the canonical workspace identity, never sessions[0]', () => {
    const sessions = [
      { id: 's1', title: 'workspace A', provider: 'p', model: 'm', state: 'idle' },
      { id: 's2', title: 'workspace B', provider: 'p', model: 'm', state: 'idle' },
    ];
    const keyA = wb.canonicalWorkspaceKey('file:///repo/a/');
    const keyB = wb.canonicalWorkspaceKey('file:///repo/b');
    assertEqual(keyA, 'file:///repo/a');
    assertEqual(wb.boundSessionFor(keyB, sessions, { [keyB]: 's2' }), 's2');
    assertEqual(
      wb.boundSessionFor(keyA, sessions, { [keyB]: 's2' }),
      null,
      'no binding must create, not reuse sessions[0]',
    );
    assertEqual(
      wb.boundSessionFor(keyB, sessions, { [keyB]: 's9' }),
      null,
      'a stale binding must not fall back to sessions[0]',
    );
    assertEqual(wb.boundSessionFor(null, sessions, { '': 's2' }), null);
    assertEqual(wb.windowWorkspaceKey([]), null);
    assertEqual(wb.windowWorkspaceKey(['file:///repo/b/', 'file:///repo/a']), 'file:///repo/b');
    const bound = wb.withBinding({}, keyB, 's2');
    assertEqual(bound[keyB], 's2');
    assertDeepEqual(wb.pruneBindings({ [keyA]: 's1', [keyB]: 'gone' }, [sessions[0]]), {
      [keyA]: 's1',
    });
    let many = {};
    for (let i = 0; i < wb.MAX_SESSION_BINDINGS + 5; i += 1) {
      many = wb.withBinding(many, `file:///w/${i}`, `s${i}`);
    }
    assertEqual(Object.keys(many).length, wb.MAX_SESSION_BINDINGS, 'bindings stay bounded');
  });
}

async function childInspectionTests() {
  await test('child summaries surface identity/worktree/ownership/capabilities/progress/result/budget/model metadata', () => {
    const catalog = [{ provider: 'fake', model: 'm', reasoning: true, thinking: true, tools: true }];
    const summaries = st.summarizeAgents(clone(agentsJson), catalog);
    const child = summaries.find((agent) => agent.agentId === 'c1');
    assertEqual(child.itemId, 'main');
    assertEqual(child.itemKind, 'Implementation');
    assertEqual(child.worktreeId, 2);
    assertEqual(child.sessionId, 8);
    assertEqual(child.ownership, 'orchestrator');
    assertDeepEqual(child.capabilities, ['ReadWorkspace']);
    assertDeepEqual(child.progress, { phase: 'work' });
    assertEqual(child.result, null);
    assertEqual(child.budget, 1000);
    assertEqual(child.model, 'm');
    assertEqual(child.provider, 'fake');
    assertEqual(child.reasoning, true);
    assertEqual(child.thinking, true);
    assertEqual(child.presentation, 'foreground');
    assertEqual(child.pixel.childId, 'c1');
    const self = summaries.find((agent) => agent.agentId === 'r1');
    assertDeepEqual(self.itemIds, ['main']);
    assertEqual(self.provider, null);
  });

  await test('same model two providers: the catalog join is (provider, model), never model alone', () => {
    const catalog = [
      { provider: 'alpha', model: 'm', reasoning: true, thinking: false, tools: true },
      { provider: 'beta', model: 'm', reasoning: false, thinking: true, tools: false },
    ];
    const frame = (agentId, provider) => ({
      ...clone(agentsJson[1]),
      agent_id: agentId,
      provider,
    });
    const summaries = st.summarizeAgents([frame('a', 'alpha'), frame('b', 'beta')], catalog);
    assertEqual(summaries[0].provider, 'alpha');
    assertEqual(summaries[0].reasoning, true, 'alpha child keeps alpha reasoning');
    assertEqual(summaries[0].thinking, false);
    assertEqual(summaries[1].provider, 'beta');
    assertEqual(summaries[1].reasoning, false, 'beta child must never inherit alpha metadata');
    assertEqual(summaries[1].thinking, true);
    // A provider-less entry never guesses metadata by model alone.
    const bare = clone(agentsJson[1]);
    delete bare.provider;
    const [legacy] = st.summarizeAgents([bare], catalog);
    assertEqual(legacy.provider, null);
    assertEqual(legacy.reasoning, null);
    assertEqual(legacy.thinking, null);
  });

  await test('background presentation survives the summary and defaults to foreground', () => {
    const [background] = st.summarizeAgents([
      { ...clone(agentsJson[1]), presentation: 'background' },
    ]);
    assertEqual(background.presentation, 'background');
    const [missing] = st.summarizeAgents([clone(agentsJson[1])]);
    assertEqual(missing.presentation, 'foreground');
    const [hostile] = st.summarizeAgents([
      { ...clone(agentsJson[1]), presentation: 'invisible' },
    ]);
    assertEqual(hostile.presentation, 'foreground', 'unknown tags never fabricate background');
  });

  await test('presentation transitions use the session-scoped route and surface typed 409s', async () => {
    const { client, calls } = makeClient({
      'POST /native/session/7/agents/c1/presentation': () => jsonResponse(presentationAckJson),
    });
    const ack = await client.setAgentPresentation('7', 'c1', 'background');
    assertEqual(ack.child_id, 'c1');
    assertEqual(ack.changed, true);
    assertDeepEqual(findCall(calls, 'POST', '/native/session/7/agents/c1/presentation').body, {
      state: 'background',
    });
    const terminal = makeClient({
      'POST /native/session/7/agents/c1/presentation': () =>
        new Response(
          JSON.stringify({
            error: { code: 'conflict', message: 'terminal child', retryable: false },
          }),
          { status: 409 },
        ),
    });
    await assertRejects(
      () => terminal.client.setAgentPresentation('7', 'c1', 'foreground'),
      (error) =>
        error instanceof nc.NativeApiError && error.status === 409 && error.code === 'conflict',
      'typed terminal presentation refusal',
    );
  });

  await test('child summary fields are bounded and blocker fields surface when present', () => {
    const huge = {
      ...clone(agentsJson[1]),
      blockers: [{ id: 'b1', detail: 'dependency x not done' }],
      progress: { phase: 'work', note: 'x'.repeat(10_000) },
    };
    const [child] = st.summarizeAgents([huge]);
    assertEqual(child.blockers[0], 'dependency x not done');
    assertEqual(child.progress.note.length < 1000, true, 'progress strings must be bounded');
    // The durable blocker object shape (kind/reason/resolution) surfaces too.
    const blocked = {
      ...clone(agentsJson[1]),
      blocker: {
        kind: 'permission',
        reason: 'waiting for a pending permission decision',
        resolution: 'resolve the pending permission request',
      },
    };
    const [durable] = st.summarizeAgents([blocked]);
    assert(
      durable.blockers[0].includes('permission: waiting for a pending permission decision'),
      durable.blockers[0],
    );
    assert(nc.validateAgents([blocked])[0].blocker.kind === 'permission', 'object blocker must validate');
  });

  await test('agent Retry uses the server guard: a typed 409 surfaces, never a silent no-op', async () => {
    const { client } = makeClient({
      'POST /native/agents/c1/retry': () =>
        new Response(
          JSON.stringify({ error: { code: 'conflict', message: 'only Failed children retry', retryable: false } }),
          { status: 409 },
        ),
    });
    await assertRejects(
      () => client.retryAgent('c1'),
      (error) => error instanceof nc.NativeApiError && error.status === 409,
      'typed retry refusal',
    );
  });
}

async function pixelAgentTests() {
  await test('pixel avatars are deterministic per ChildId with per-state animation', () => {
    const a1 = px.pixelAvatar('c1');
    const a2 = px.pixelAvatar('c1');
    assertDeepEqual(a1, a2);
    assertEqual(a1.pixels.length, 25);
    assertEqual(a1.pixels.filter((bit) => bit === 1).length > 0, true, 'sprite must have pixels');
    assert(
      a1.hash !== px.pixelAvatar('c2').hash || a1.color !== px.pixelAvatar('c2').color,
      'different children must differ deterministically',
    );
    const mapping = [
      ['Running', 'running'],
      ['Paused', 'paused'],
      ['Waiting', 'waiting'],
      ['Blocked', 'blocked'],
      ['Done', 'done'],
      ['Failed', 'failed'],
      ['Cancelled', 'cancelled'],
      ['Idle', 'waiting'],
    ];
    for (const [native, expected] of mapping) {
      assertEqual(px.pixelStateOf(native), expected, native);
    }
    assertEqual(px.pixelAnimation('running'), 'pixel-running');
    assertEqual(px.pixelAnimation('blocked'), 'pixel-blocked');
  });

  await test('pixel presence transitions over native mock frames and survives gaps', () => {
    const frame = (state) => [
      {
        agent_id: 'c1',
        kind: 'child',
        run_id: 'r1',
        session_id: 8,
        worktree_id: 2,
        goal: 'g',
        state,
        model: 'm',
        budget: 1,
        ownership: 'isolated_worktree',
        capabilities: [],
        progress: null,
        result: null,
        item_id: 'main',
        item_kind: 'Implementation',
      },
    ];
    let presence = new Map();
    const timeline = [];
    for (const state of ['Running', 'Paused', 'Waiting', 'Blocked', 'Done']) {
      presence = px.foldPixelPresence(presence, frame(state));
      const entry = presence.get('c1');
      timeline.push(entry.state);
      assertDeepEqual(entry.avatar, px.pixelAvatar('c1'), 'avatar is stable across frames');
      assertEqual(entry.animation, `pixel-${entry.state}`);
    }
    assertDeepEqual(timeline, ['running', 'paused', 'waiting', 'blocked', 'done']);
    presence = px.foldPixelPresence(presence, []);
    assertEqual(presence.get('c1').state, 'done', 'a frame gap keeps the last presence');
  });
}

async function cockpitTests() {
  await test('validateTaskViews surfaces additive criteria/plan/blockers/evidence/phase fields', () => {
    const payload = {
      ...clone(taskViewJson),
      acceptance_criteria: ['build passes', 'tests pass'],
      plan: [{ id: 'main', summary: 'implement', state: 'running', depends_on: ['analysis'] }],
      blockers: [{ id: 'b1', detail: 'waiting on analysis', state: 'open' }],
      evidence_refs: ['evidence:41'],
      phase: 'implementation',
    };
    const view = nc.validateTaskViews([payload])[0];
    assertDeepEqual(view.acceptanceCriteria, ['build passes', 'tests pass']);
    assertEqual(view.plan[0].id, 'main');
    assertDeepEqual(view.plan[0].dependsOn, ['analysis']);
    assertEqual(view.blockers[0].detail, 'waiting on analysis');
    assertDeepEqual(view.evidenceRefs, ['evidence:41']);
    assertEqual(view.phase, 'implementation');
    const bare = nc.validateTaskViews([clone(taskViewJson)])[0];
    assertDeepEqual(bare.acceptanceCriteria, []);
    assertDeepEqual(bare.plan, []);
    assertDeepEqual(bare.blockers, []);
    assertEqual(bare.phase, null);
    assertProtocol(
      () => nc.validateTaskViews([{ ...clone(taskViewJson), plan: [{ id: 'x', depends_on: [7] }] }]),
      'expected a string',
    );
    assertProtocol(() => nc.validateTaskViews([{ ...clone(taskViewJson), blockers: 'nope' }]), 'expected an array');
  });

  await test('task cockpit renders every section from a mock native payload', () => {
    const taskView = nc.validateTaskViews([
      {
        ...clone(taskViewJson),
        acceptance_criteria: ['build passes', 'tests pass'],
        plan: [
          { id: 'analysis', summary: 'analyze', state: 'done' },
          { id: 'main', summary: 'implement', state: 'running', depends_on: ['analysis'] },
        ],
        blockers: [{ id: 'b1', detail: 'waiting on analysis', state: 'open' }],
        evidence_refs: [],
        phase: 'implementation',
      },
    ])[0];
    const task = {
      goal: taskView.goal,
      state: taskView.state,
      completed: taskView.milestones.completed,
      open: taskView.milestones.open,
      testsRun: taskView.tests.run,
      testsFailed: taskView.tests.failed,
      changedFiles: taskView.changedFiles,
      budget: taskView.budget,
      acceptanceCriteria: taskView.acceptanceCriteria,
      plan: taskView.plan,
      blockers: taskView.blockers.map((blocker) => blocker.detail),
      evidenceRefs: taskView.evidenceRefs,
      phase: taskView.phase,
      progress: taskView.progress,
    };
    const catalog = [{ provider: 'fake', model: 'm', reasoning: true, thinking: true, tools: true }];
    const agents = st.summarizeAgents(
      clone(agentsJson).map((agent) =>
        agent.kind === 'child' ? { ...agent, state: 'Blocked' } : agent,
      ),
      catalog,
    );
    const verification = clone(verificationViewJson);
    const usage = clone(sessionUsageJson);
    const taskVerificationRecord = clone(verificationRecordJson);
    taskVerificationRecord.criteria[0].evidence = 'evidence:41';
    const view = cp.buildCockpit({
      task,
      agents,
      verification,
      usage: {
        tokens: usage.providerCalls.tokens,
        spentMicro: '12',
        maxMicro: null,
        openMicro: '0',
        truncated: false,
      },
      taskVerification: { records: [taskVerificationRecord] },
    });
    assert(view, 'a task cockpit must build from the mock payload');
    const sections = cp.cockpitSections(view);
    assertDeepEqual(
      sections.map((section) => section.key),
      ['acceptance', 'proof', 'plan', 'completion', 'children', 'tournament', 'phase', 'blockers', 'verification', 'evidence', 'spend'],
    );
    const byKey = Object.fromEntries(sections.map((section) => [section.key, section]));
    assert(byKey.acceptance.present && byKey.acceptance.lines.some((line) => line.includes('build passes')));
    assert(
      byKey.plan.present &&
        byKey.plan.lines[0].includes('analysis') &&
        byKey.plan.lines[1].includes('after analysis'),
      JSON.stringify(byKey.plan.lines),
    );
    assert(
      byKey.children.present &&
        byKey.children.lines[0].includes('c1') &&
        byKey.children.lines[0].includes('worktree 2') &&
        byKey.children.lines[0].includes('ownership orchestrator'),
      JSON.stringify(byKey.children.lines),
    );
    assert(byKey.phase.present && byKey.phase.lines[0].includes('implementation'));
    assert(byKey.blockers.present && byKey.blockers.lines[0].includes('waiting on analysis'));
    assert(byKey.verification.present && byKey.verification.lines[0].includes('status passed'));
    // The top-level summary renders VERIFIED only from a served record that
    // proves the full criterion set, and carries the served review/tree/
    // commit/remote-head/cost facts alongside criteria and checks.
    assert(
      byKey.verification.lines[1].startsWith('VERIFIED') &&
        byKey.verification.lines[1].includes('criteria 1/1') &&
        byKey.verification.lines[1].includes('checks failed 0') &&
        byKey.verification.lines[1].includes('review review-bot') &&
        byKey.verification.lines[1].includes('tree ver1234') &&
        byKey.verification.lines[1].includes('commit abcdef12') &&
        byKey.verification.lines[1].includes('head refs/9') &&
        byKey.verification.lines[1].includes('cost 0.0000'),
      JSON.stringify(byKey.verification.lines),
    );
    assert(
      byKey.acceptance.lines.some((line) => line.includes('commit abcdef12')),
      JSON.stringify(byKey.acceptance.lines),
    );
    assert(
      byKey.evidence.present && byKey.evidence.evidence[0].id === 41,
      JSON.stringify(byKey.evidence),
    );
    assert(byKey.spend.present && byKey.spend.lines[0].includes('tokens 130'));
    assertEqual(cp.evidenceRefOf('evidence:41').id, 41);
    assertEqual(cp.evidenceRefOf('plain text').id, null);
    assertEqual(cp.phaseOf({ stage: 'verify' }), 'verify');
    assertEqual(cp.buildCockpit({ task: null, agents: [], verification: null, usage: null, taskVerification: null }), null);
  });

  await test('cockpit tucks background children last and marks them dimmed', () => {
    const [backgroundChild] = st.summarizeAgents([
      { ...clone(agentsJson[1]), presentation: 'background' },
    ]);
    const [foregroundChild] = st.summarizeAgents([clone(agentsJson[1])]);
    const view = cp.buildCockpit({
      task: null,
      agents: [backgroundChild, foregroundChild],
      verification: null,
      usage: null,
      taskVerification: null,
    });
    assertDeepEqual(
      view.children.map((child) => child.presentation),
      ['foreground', 'background'],
      'background children are ordered after the foreground ones',
    );
    const children = cp.cockpitSections(view).find((section) => section.key === 'children');
    assert(
      children.lines[1].includes('background (dimmed)'),
      JSON.stringify(children.lines),
    );
  });

  await test('cockpit tournament block is state-gated: decide waits, abort is open-only', () => {
    const candidate = (childId, state) => ({
      childId,
      state,
      verification: state === 'done' ? 12 : null,
      verificationPass: state === 'done' ? true : null,
      reviewRank: state === 'done' ? 'clean' : null,
      reviewer: null,
      costMicro: 1n,
      wallMs: 2,
    });
    const native = (state, winner, candidates) => ({
      id: 't-1',
      state,
      winner,
      criteria: [{ id: 'c-1', spec: 'tests pass' }],
      candidates,
    });
    const running = cp.tournamentViewOf(
      native('open', null, [candidate('child-0', 'done'), candidate('child-1', 'running')]),
    );
    assertEqual(running.open, true);
    assertEqual(running.canDecide, false, 'decide waits until every candidate settled');
    const settled = cp.tournamentViewOf(
      native('open', null, [candidate('child-0', 'done'), candidate('child-1', 'done')]),
    );
    assertEqual(settled.canDecide, true);
    const decided = cp.tournamentViewOf(
      native('decided', 'child-0', [candidate('child-0', 'done'), candidate('child-1', 'discarded')]),
    );
    assertEqual(decided.open, false);
    assertEqual(decided.canDecide, false, 'a decided tournament exposes no controls');

    // A tournament ALONE builds a cockpit and carries its section + actions.
    const view = cp.buildCockpit({
      task: null,
      agents: [],
      verification: null,
      usage: null,
      taskVerification: null,
      tournament: running,
    });
    assert(view, 'a tournament alone must build a cockpit');
    assert(view.tournament, 'the cockpit must carry the tournament block');
    const section = cp.cockpitSections(view).find((entry) => entry.key === 'tournament');
    assert(section.present, 'the tournament section is present');
    assert(
      section.lines.some((line) => line.includes('t-1') && line.includes('[open]')),
      JSON.stringify(section.lines),
    );
    const decide = section.actions.find((action) => action.key === 'decide');
    const abort = section.actions.find((action) => action.key === 'abort');
    assertEqual(decide.enabled, false, 'decide action is disabled while running');
    assertEqual(abort.enabled, true, 'abort action stays available while open');
    // The decided view's gating survives the section projection.
    const decidedView = cp.buildCockpit({
      task: null,
      agents: [],
      verification: null,
      usage: null,
      taskVerification: null,
      tournament: decided,
    });
    const decidedSection = cp.cockpitSections(decidedView).find(
      (entry) => entry.key === 'tournament',
    );
    assertEqual(
      decidedSection.actions.every((action) => action.enabled === false),
      true,
      'terminal tournaments expose only disabled controls',
    );
  });
}

// ------------------------------------------- acceptance-criterion proof

function proofDigest(seed) {
  return String(seed).repeat(64).slice(0, 64);
}

function proofRecordJson(overrides = {}) {
  return {
    recordId: 'rec-proof',
    revision: 'rev-9',
    workspaceId: '1',
    worktreeId: '1',
    treeHash: proofDigest('f'),
    criteria: [],
    checks: [],
    changedFiles: [],
    unrelatedChanges: [],
    reviewer: null,
    status: 'passed',
    startedMs: 1700000000000,
    completedMs: 1700000002000,
    candidateProof: {
      taskRevision: 'rev-9',
      baseManifestHash: proofDigest('a'),
      candidateManifestHash: proofDigest('b'),
      sourceDiffEvidence: 'evidence:7',
      riskReportEvidence: null,
      accountingSnapshotDigest: 'accounting:v1:feedface',
      runId: 'run-proof',
      runBaseSnapshot: proofDigest('1'),
      candidateSnapshot: proofDigest('2'),
      sourcesDigest: proofDigest('3'),
      changedFilesDigest: proofDigest('4'),
    },
    verifiedSnapshot: proofDigest('2'),
    basedOnSnapshot: proofDigest('1'),
    sourceCount: 3,
    landedSnapshot: proofDigest('5'),
    ...overrides,
  };
}

function proofTask(acceptanceCriteria = []) {
  return {
    goal: 'prove the criteria',
    state: 'verifying',
    completed: [],
    open: [],
    testsRun: [],
    testsFailed: [],
    changedFiles: [],
    budget: null,
    acceptanceCriteria,
    plan: [],
    blockers: [],
    evidenceRefs: [],
    phase: null,
    progress: null,
    completion: null,
  };
}

function proofCockpit(taskVerification, acceptanceCriteria = []) {
  return cp.buildCockpit({
    task: proofTask(acceptanceCriteria),
    agents: [],
    verification: null,
    usage: null,
    taskVerification,
  });
}

const BINDING_JSON = {
  required_check: { kind: 'required_check', check_id: 'rust_check', command_digest: 'digest-check' },
  integration_coverage: {
    kind: 'integration_coverage',
    required_work_items: ['impl-a', 'impl-b'],
  },
  file_state: { kind: 'file_state', path: 'src/a.rs', expected_digest: 'digest-file' },
  evidence: { kind: 'evidence', evidence_id: '41', evidence_digest: 'digest-evidence' },
  independent_review: { kind: 'independent_review', reviewer_id: 'reviewer-1' },
  aggregate_goal: { kind: 'aggregate_goal' },
  unavailable: { kind: 'unavailable', reason: 'no objective mechanism' },
};

async function acceptanceProofTests() {
  await test('native client parses the additive proof payload and degrades hostile annotations', () => {
    const record = proofRecordJson({
      criteria: [
        {
          criterionKey: 'c1',
          passed: true,
          evidence: 'evidence:41',
          requirement: 'required',
          origin: 'user',
          verdict: 'passed',
          binding: BINDING_JSON.required_check,
        },
      ],
    });
    const view = nc.validateTaskVerification({ sessionId: '7', taskId: '3', records: [record] });
    const parsed = view.records[0];
    assertEqual(parsed.candidateProof.runId, 'run-proof');
    assertEqual(parsed.candidateProof.candidateSnapshot, proofDigest('2'));
    assertEqual(parsed.verifiedSnapshot, proofDigest('2'));
    assertEqual(parsed.basedOnSnapshot, proofDigest('1'));
    assertEqual(parsed.sourceCount, 3);
    assertEqual(parsed.landedSnapshot, proofDigest('5'));
    assertEqual(parsed.criteria[0].binding.kind, 'required_check');
    assertEqual(parsed.criteria[0].binding.checkId, 'rust_check');
    assertEqual(parsed.criteria[0].binding.commandDigest, 'digest-check');
    assertEqual(parsed.criteria[0].requirement, 'required');
    assertEqual(parsed.criteria[0].origin, 'user');
    assertEqual(parsed.criteria[0].verdict, 'passed');
    // The committed daemon shape (proof keys present, nulls) parses too.
    const committed = nc.validateTaskVerification({
      sessionId: '7',
      taskId: '3',
      records: [
        proofRecordJson({
          candidateProof: null,
          verifiedSnapshot: null,
          basedOnSnapshot: null,
          sourceCount: null,
          landedSnapshot: null,
        }),
      ],
    }).records[0];
    assertEqual(committed.candidateProof, null);
    assertEqual(committed.verifiedSnapshot, null);
    // A daemon predating the proof payload is tolerated (absent = null).
    const old = nc.validateTaskVerification({
      sessionId: '7',
      taskId: '3',
      records: [
        {
          ...proofRecordJson(),
          candidateProof: undefined,
          verifiedSnapshot: undefined,
          basedOnSnapshot: undefined,
          sourceCount: undefined,
          landedSnapshot: undefined,
          criteria: [{ criterionKey: 'legacy', passed: true, evidence: null }],
        },
      ],
    }).records[0];
    assertEqual(old.verifiedSnapshot, null);
    assertEqual(old.criteria[0].binding, null);
    // Hostile annotations degrade per row: wrong types never throw and never pass.
    const hostile = nc.validateTaskVerification({
      sessionId: '7',
      taskId: '3',
      records: [
        proofRecordJson({
          candidateProof: 'not-an-object',
          verifiedSnapshot: 42,
          sourceCount: 'three',
          criteria: [{}, { criterionKey: 7, passed: 'yes' }, { criterionKey: 'ok', passed: false }],
        }),
      ],
    }).records[0];
    assertEqual(hostile.candidateProof, null);
    assertEqual(hostile.verifiedSnapshot, null);
    assertEqual(hostile.sourceCount, null);
    assertEqual(hostile.criteria[0].criterionKey, '');
    assertEqual(hostile.criteria[0].passed, null);
    assertEqual(hostile.criteria[1].passed, null);
    assertEqual(hostile.criteria[2].passed, false);
  });

  await test('every binding kind renders per criterion with its exact reference', () => {
    const kinds = Object.keys(BINDING_JSON);
    const record = proofRecordJson({
      criteria: [
        {
          criterionKey: 'check criterion',
          passed: true,
          evidence: 'check:rust_check:digest-check',
          requirement: 'required',
          origin: 'project_policy',
          verdict: 'passed',
          binding: BINDING_JSON.required_check,
        },
        {
          criterionKey: 'coverage criterion',
          passed: true,
          evidence: null,
          requirement: 'required',
          origin: 'user',
          binding: BINDING_JSON.integration_coverage,
        },
        {
          criterionKey: 'file criterion',
          passed: false,
          evidence: 'file:src/a.rs',
          requirement: 'required',
          origin: 'verification_policy',
          verdict: 'failed',
          binding: BINDING_JSON.file_state,
        },
        {
          criterionKey: 'evidence criterion',
          passed: true,
          evidence: 'evidence:41',
          requirement: 'required',
          origin: 'user',
          binding: BINDING_JSON.evidence,
        },
        {
          criterionKey: 'review criterion',
          passed: null,
          evidence: null,
          requirement: 'preferred',
          origin: 'semantic_provider',
          verdict: 'unavailable',
          binding: BINDING_JSON.independent_review,
        },
        {
          criterionKey: 'goal criterion',
          passed: true,
          evidence: null,
          requirement: 'required',
          origin: 'user',
          binding: BINDING_JSON.aggregate_goal,
        },
        {
          criterionKey: 'unavailable criterion',
          passed: false,
          evidence: null,
          requirement: 'required',
          origin: 'user',
          verdict: 'unavailable',
          binding: BINDING_JSON.unavailable,
        },
      ],
    });
    const view = nc.validateTaskVerification({ sessionId: '7', taskId: '3', records: [record] });
    const cockpit = proofCockpit(view);
    assertEqual(cockpit.criteriaProof.length, 7);
    assertDeepEqual(cockpit.criteriaProof.map((row) => row.binding), kinds);
    assert(
      cockpit.criteriaProof.every((row) => row.bindingSource === 'daemon'),
      'served bindings win',
    );
    assertEqual(cockpit.criteriaProof[0].bindingReference, 'check:rust_check:digest-check');
    assertEqual(cockpit.criteriaProof[1].bindingReference, 'work-item:impl-a, work-item:impl-b');
    assertEqual(cockpit.criteriaProof[2].bindingReference, 'src/a.rs');
    assertEqual(cockpit.criteriaProof[3].bindingReference, 'evidence:41');
    assertEqual(cockpit.criteriaProof[4].bindingReference, 'reviewer-1');
    assertEqual(cockpit.criteriaProof[5].bindingReference, null);
    assertDeepEqual(
      cockpit.criteriaProof.map((row) => row.verdict),
      ['pass', 'pass', 'fail', 'pass', 'unavailable', 'pass', 'unavailable'],
    );
    assertDeepEqual(
      cockpit.criteriaProof.map((row) => row.requirement),
      ['required', 'required', 'required', 'required', 'preferred', 'required', 'required'],
    );
    assertDeepEqual(
      cockpit.criteriaProof.map((row) => row.origin),
      ['project_policy', 'user', 'verification_policy', 'user', 'semantic_provider', 'user', 'user'],
    );
    const section = cp.cockpitSections(cockpit).find((entry) => entry.key === 'acceptance');
    assertEqual(section.criteria.length, 7);
    assert(section.lines[0].startsWith('[pass] check criterion'), section.lines[0]);
    assert(
      section.lines.some((line) => line.startsWith('[fail] file criterion')),
      JSON.stringify(section.lines),
    );
    assert(
      section.lines.some((line) => line.startsWith('[unavailable] review criterion')),
      JSON.stringify(section.lines),
    );
    assert(
      section.lines[0].includes('binding required_check ref check:rust_check:digest-check'),
      section.lines[0],
    );
  });

  await test('binding kinds are derived from typed evidence refs only when unambiguous', () => {
    const record = proofRecordJson({
      criteria: [
        { criterionKey: 'd-check', passed: true, evidence: 'check:rust_check:digest-check' },
        { criterionKey: 'd-file', passed: true, evidence: 'file:src/a.rs' },
        { criterionKey: 'd-evidence', passed: true, evidence: 'evidence:42' },
        {
          criterionKey: 'd-coverage',
          passed: true,
          evidence: 'work-item:impl-a:digest-a ; work-item:impl-b:digest-b',
        },
        { criterionKey: 'd-mixed', passed: true, evidence: 'check:rust_check:digest-check ; file:src/a.rs' },
        { criterionKey: 'd-none', passed: false, evidence: 'the reviewer refused to certify' },
      ],
    });
    const view = nc.validateTaskVerification({ sessionId: '7', taskId: '3', records: [record] });
    const rows = proofCockpit(view).criteriaProof;
    assertDeepEqual(
      rows.slice(0, 4).map((row) => row.binding),
      ['required_check', 'file_state', 'evidence', 'integration_coverage'],
    );
    assert(
      rows.slice(0, 4).every((row) => row.bindingSource === 'derived'),
      'derived provenance is explicit',
    );
    assertEqual(rows[0].bindingReference, 'check:rust_check:digest-check');
    assertEqual(rows[1].bindingReference, 'file:src/a.rs');
    assertEqual(rows[2].bindingReference, 'evidence:42');
    assertEqual(rows[3].bindingReference, 'work-item:impl-a:digest-a, work-item:impl-b:digest-b');
    // Mixed kinds are never guessed (an aggregate would look mixed).
    assertEqual(rows[4].binding, 'unavailable');
    assertEqual(rows[4].bindingSource, 'unavailable');
    // A prose evidence string with no typed ref identifies no binding.
    assertEqual(rows[5].binding, 'unavailable');
    assert(rows[5].verdict === 'fail', 'a recorded false is a fail, never a pass');
  });

  await test('missing and hostile proof payloads degrade to honest unavailable rows', () => {
    // No verification record at all: every explicit criterion is unavailable.
    const bare = proofCockpit(null, ['first criterion', 'second criterion']);
    assertEqual(bare.criteriaProof.length, 2);
    assert(
      bare.criteriaProof.every((row) => row.verdict === 'unavailable'),
      'no record => no pass',
    );
    assert(bare.criteriaProof.every((row) => row.binding === 'unavailable'));
    assert(bare.criteriaProof.every((row) => row.unavailable.includes('binding')));
    assert(bare.criteriaProof.every((row) => row.unavailable.includes('verification timestamp')));
    // A hostile record: malformed rows never throw and never pass.
    const hostile = proofCockpit({
      records: [
        {
          recordId: 'r',
          status: 'passed',
          startedMs: 1,
          completedMs: 2,
          criteria: [{}, { criterionKey: 'x', passed: 'yes' }, { criterionKey: 'y', passed: false }],
          checks: [],
        },
      ],
    });
    assert(
      hostile.criteriaProof.every((row) => row.verdict !== 'pass'),
      'hostile rows never fake a pass',
    );
    assertEqual(hostile.criteriaProof[0].criterionKey, '');
    assertEqual(hostile.criteriaProof[1].verdict, 'unavailable');
    assertEqual(hostile.criteriaProof[2].verdict, 'fail');
    assert(
      String(hostile.criteriaProof[2].verdictReason).includes('cannot be distinguished'),
      hostile.criteriaProof[2].verdictReason,
    );
    // An unrecognized explicit verdict annotation does not override a
    // recorded pass fact (the recorded boolean is the served proof).
    const unknownVerdict = proofCockpit({
      records: [
        {
          recordId: 'r',
          status: 'passed',
          startedMs: 1,
          completedMs: 2,
          criteria: [{ criterionKey: 'z', passed: true, verdict: 'probably-fine' }],
          checks: [],
        },
      ],
    });
    assertEqual(unknownVerdict.criteriaProof[0].verdict, 'pass');
  });

  await test('proof rows carry the proven snapshots and verification timestamps', () => {
    const view = nc.validateTaskVerification({
      sessionId: '7',
      taskId: '3',
      records: [proofRecordJson({ criteria: [{ criterionKey: 'snap', passed: true, evidence: 'check:c:1' }] })],
    });
    const cockpit = proofCockpit(view);
    const row = cockpit.criteriaProof[0];
    assertEqual(row.snapshot.candidate, proofDigest('2'));
    assertEqual(row.snapshot.verified, proofDigest('2'));
    assertEqual(row.snapshot.basedOn, proofDigest('1'));
    assertEqual(row.snapshot.landed, proofDigest('5'));
    assertEqual(row.snapshot.sourceCount, 3);
    assertEqual(row.snapshot.runId, 'run-proof');
    assertEqual(row.timestamp.startedMs, 1700000000000);
    assertEqual(row.timestamp.completedMs, 1700000002000);
    assertEqual(row.recordId, 'rec-proof');
    const section = cp.cockpitSections(cockpit).find((entry) => entry.key === 'acceptance');
    assert(section.lines[0].includes('cand '), section.lines[0]);
    assert(section.lines[0].includes('ver '), section.lines[0]);
    assert(section.lines[0].includes('base '), section.lines[0]);
    assert(section.lines[0].includes('land '), section.lines[0]);
    assert(section.lines[0].includes('src 3'), section.lines[0]);
    assert(section.lines[0].includes('2023-11-14T22:13:20Z'), section.lines[0]);
  });

  await test('fallback webview renders distinct pass/fail/unavailable proof rows with retrieval', () => {
    const record = proofRecordJson({
      criteria: [
        { criterionKey: 'passes', passed: true, evidence: 'check:rust_check:digest-check' },
        { criterionKey: 'fails', passed: false, evidence: 'check:rust_check:digest-check' },
        { criterionKey: 'unknown', passed: null, evidence: 'the reviewer was unavailable' },
        { criterionKey: 'certified', passed: true, evidence: 'evidence:42 tool output' },
      ],
    });
    const view = nc.validateTaskVerification({ sessionId: '7', taskId: '3', records: [record] });
    const cockpit = proofCockpit(view);
    const snapshot = webviewSnapshot([]);
    snapshot.cockpit = cockpit;
    snapshot.cockpitSections = cp.cockpitSections(cockpit);
    const { posted, dom } = runChatWebview(snapshot);
    const cockpitNode = dom.document.getElementById('cockpit');
    const rowFor = (verdict) =>
      findFake(cockpitNode, (node) => node.getAttribute('data-verdict') === verdict);
    const passRow = rowFor('pass');
    const failRow = rowFor('fail');
    const unavailableRow = rowFor('unavailable');
    assert(passRow && failRow && unavailableRow, 'all three verdict rows must render');
    assert(passRow !== failRow && failRow !== unavailableRow, 'verdict states are distinct rows');
    assert(String(passRow.className).includes('criterion-pass'), passRow.className);
    assert(String(failRow.className).includes('criterion-fail'), failRow.className);
    assert(String(unavailableRow.className).includes('criterion-unavailable'), unavailableRow.className);
    const passText = fakeText(passRow);
    assert(passText.includes('binding required_check (derived)'), passText);
    assert(passText.includes('snapshot candidate'), passText);
    assert(passText.includes('verified at'), passText);
    const unavailableText = fakeText(unavailableRow);
    assert(unavailableText.includes('UNAVAILABLE'), unavailableText);
    assert(unavailableText.includes('unavailable: requirement'), unavailableText);
    // The typed evidence ref stays clickable (retrieval goes to the host).
    const certified = findFake(cockpitNode, (node) => fakeText(node).includes('certified'));
    const button = findFake(
      certified,
      (node) => node.tagName === 'button' && node.textContent === 'View evidence #42',
    );
    assert(button, 'an evidence:<n> ref must render a retrieval button');
    button.click();
    assertDeepEqual(posted[posted.length - 1], { type: 'retrieveEvidence', evidenceId: 42 });
  });
}

// ----------------------------------------- presentation webview (fake DOM)

function makeFakeDom() {
  const nodesById = new Map();
  function makeNode(tagName) {
    const node = {
      tagName,
      children: [],
      parentNode: null,
      attributes: {},
      listeners: {},
      className: '',
      textContent: '',
      scrollTop: 0,
      scrollHeight: 0,
      hidden: false,
      value: '',
      type: '',
    };
    node.appendChild = (child) => {
      node.children.push(child);
      child.parentNode = node;
      return child;
    };
    node.removeChild = (child) => {
      const index = node.children.indexOf(child);
      if (index >= 0) {
        node.children.splice(index, 1);
      }
      return child;
    };
    node.setAttribute = (key, value) => {
      node.attributes[key] = String(value);
    };
    node.getAttribute = (key) => (key in node.attributes ? node.attributes[key] : null);
    node.addEventListener = (type, callback) => {
      if (!node.listeners[type]) {
        node.listeners[type] = [];
      }
      node.listeners[type].push(callback);
    };
    node.dispatch = (type, event) => {
      for (const callback of node.listeners[type] || []) {
        callback(event || {});
      }
    };
    node.click = () => node.dispatch('click', {});
    node.cloneNode = () => {
      const copy = makeNode(tagName);
      copy.className = node.className;
      copy.textContent = node.textContent;
      return copy;
    };
    Object.defineProperty(node, 'firstChild', { get: () => node.children[0] || null });
    Object.defineProperty(node, 'childNodes', { get: () => node.children });
    return node;
  }
  const document = {
    getElementById(id) {
      if (!nodesById.has(id)) {
        nodesById.set(id, makeNode('div'));
      }
      return nodesById.get(id);
    },
    createElement: makeNode,
    createElementNS: (namespace, tagName) => makeNode(tagName),
    querySelectorAll: () => [],
  };
  return { document, nodesById, makeNode };
}

function walkFake(root, visit) {
  visit(root);
  for (const child of root.children || []) {
    walkFake(child, visit);
  }
}

function findFake(root, predicate) {
  let found = null;
  walkFake(root, (node) => {
    if (found === null && predicate(node)) {
      found = node;
    }
  });
  return found;
}

/** Concatenated text of a fake-DOM subtree (assertions only). */
function fakeText(root) {
  let out = '';
  walkFake(root, (node) => {
    if (node.textContent) {
      out += `${node.textContent} `;
    }
  });
  return out;
}

function webviewSnapshot(agents) {
  return {
    daemon: 'running',
    daemonDetail: 'selftest',
    baseUrl: 'http://127.0.0.1:9',
    session: clone(sessionSummaryJson),
    machineState: 'idle',
    machineLabel: 'Idle',
    sessions: [clone(sessionSummaryJson)],
    runs: [],
    activeRunId: null,
    agents,
    task: null,
    verification: null,
    usage: null,
    cockpit: null,
    cockpitSections: [],
    tournament: null,
    transcript: [],
    streamStatus: 'open',
    lastError: null,
    busy: false,
  };
}

function runChatWebview(snapshot) {
  const source = readFileSync(new URL('../media/chat.js', import.meta.url), 'utf8');
  const posted = [];
  const dom = makeFakeDom();
  let messageHandler = null;
  const sandbox = {
    document: dom.document,
    window: {
      addEventListener(type, callback) {
        if (type === 'message') {
          messageHandler = callback;
        }
      },
    },
    acquireVsCodeApi: () => ({ postMessage: (message) => posted.push(message) }),
    setTimeout: () => 0,
    clearTimeout: () => {},
  };
  vm.createContext(sandbox);
  vm.runInContext(source, sandbox);
  assert(messageHandler, 'chat.js must register a window message listener');
  messageHandler({ data: { type: 'snapshot', snapshot } });
  return {
    posted,
    dom,
    deliver: (message) => messageHandler({ data: message.data ? message.data : message }),
  };
}

async function presentationWebviewTests() {
  await test('background children render dimmed+grouped and the toggle posts the next state', () => {
    const backgroundWire = clone(agentsJson).map((agent) =>
      agent.kind === 'child' ? { ...agent, presentation: 'background' } : agent,
    );
    const agents = st.summarizeAgents(nc.validateAgents(backgroundWire));
    const { posted, dom } = runChatWebview(webviewSnapshot(agents));
    const list = dom.document.getElementById('agent-list');
    const dimmed = findFake(list, (node) => String(node.className).includes('agent-background'));
    assert(dimmed, 'a background child must carry the agent-background class');
    const group = findFake(list, (node) => node.className === 'agent-group-label');
    assert(
      group && /Background \(1\)/.test(group.textContent),
      `background children must be grouped: ${group && group.textContent}`,
    );
    const title = findFake(dimmed, (node) => String(node.className).includes('agent-title'));
    assert(
      title && title.textContent.includes('background'),
      `the dimmed marker must surface in the title: ${title && title.textContent}`,
    );
    const toggle = findFake(
      dimmed,
      (node) => node.tagName === 'button' && node.textContent === 'Foreground',
    );
    assert(toggle, 'a background child must offer a Foreground toggle');
    toggle.click();
    assertDeepEqual(posted[posted.length - 1], {
      type: 'agentControl',
      agentId: 'c1',
      action: 'presentation',
      state: 'foreground',
    });

    const foreground = st.summarizeAgents(nc.validateAgents(clone(agentsJson)));
    const second = runChatWebview(webviewSnapshot(foreground));
    const secondList = second.dom.document.getElementById('agent-list');
    assert(
      !findFake(secondList, (node) => String(node.className).includes('agent-background')),
      'foreground children are never dimmed',
    );
    assert(
      !findFake(secondList, (node) => node.className === 'agent-group-label'),
      'no background group without background children',
    );
    const forward = findFake(
      secondList,
      (node) => node.tagName === 'button' && node.textContent === 'Background',
    );
    assert(forward, 'a foreground child must offer a Background toggle');
    forward.click();
    assertDeepEqual(second.posted[second.posted.length - 1], {
      type: 'agentControl',
      agentId: 'c1',
      action: 'presentation',
      state: 'background',
    });
  });
}

// ----------------------------------- tournament cockpit + reduced motion

function tournamentCockpitSnapshot(tournament) {
  const cockpit = cp.buildCockpit({
    task: null,
    agents: [],
    verification: null,
    usage: null,
    taskVerification: null,
    tournament,
  });
  const snapshot = webviewSnapshot([]);
  snapshot.cockpit = cockpit;
  snapshot.cockpitSections = cp.cockpitSections(cockpit);
  snapshot.tournament = tournament;
  return snapshot;
}

async function tournamentWebviewTests() {
  await test('cockpit Decide/Abort are state-gated and post only when enabled', () => {
    const candidate = (childId, state) => ({
      childId,
      state,
      verification: null,
      verificationPass: null,
      reviewRank: null,
      reviewer: null,
      costMicro: 0n,
      wallMs: 0,
    });
    const wire = {
      id: 't-1',
      state: 'open',
      winner: null,
      criteria: [{ id: 'c-1', spec: 'tests pass' }],
      candidates: [candidate('child-0', 'done'), candidate('child-1', 'running')],
    };
    const running = cp.tournamentViewOf(wire);
    const first = runChatWebview(tournamentCockpitSnapshot(running));
    const cockpit = first.dom.document.getElementById('cockpit');
    const decide = findFake(
      cockpit,
      (node) => node.tagName === 'button' && node.textContent === 'Decide winner',
    );
    const abort = findFake(cockpit, (node) => node.tagName === 'button' && node.textContent === 'Abort');
    assert(decide && abort, 'the tournament section must render Decide/Abort controls');
    assertEqual(decide.disabled, true, 'decide is disabled until every candidate settles');
    assertEqual(abort.disabled, false, 'abort is enabled while the tournament is open');
    const before = first.posted.length;
    decide.click();
    assertEqual(first.posted.length, before, 'a disabled Decide must never post');
    abort.click();
    assertDeepEqual(first.posted[first.posted.length - 1], {
      type: 'tournamentControl',
      tournamentId: 't-1',
      action: 'abort',
    });

    // Every candidate settled: decide enables and posts the exact control.
    const settled = cp.tournamentViewOf({
      ...wire,
      candidates: [candidate('child-0', 'done'), candidate('child-1', 'done')],
    });
    const second = runChatWebview(tournamentCockpitSnapshot(settled));
    const secondCockpit = second.dom.document.getElementById('cockpit');
    const decideNow = findFake(
      secondCockpit,
      (node) => node.tagName === 'button' && node.textContent === 'Decide winner',
    );
    assertEqual(decideNow.disabled, false, 'decide enables once all candidates settle');
    decideNow.click();
    assertDeepEqual(second.posted[second.posted.length - 1], {
      type: 'tournamentControl',
      tournamentId: 't-1',
      action: 'decide',
    });

    // A terminal tournament renders disabled controls only.
    const decided = cp.tournamentViewOf({
      ...wire,
      state: 'decided',
      winner: 'child-0',
      candidates: [candidate('child-0', 'done'), candidate('child-1', 'discarded')],
    });
    const third = runChatWebview(tournamentCockpitSnapshot(decided));
    const thirdCockpit = third.dom.document.getElementById('cockpit');
    const buttons = [];
    walkFake(thirdCockpit, (node) => {
      if (node.tagName === 'button' && (node.textContent === 'Abort' || node.textContent === 'Decide winner')) {
        buttons.push(node);
      }
    });
    assertEqual(buttons.length, 2, 'terminal tournaments still render both controls');
    assertEqual(
      buttons.every((button) => button.disabled === true),
      true,
      'terminal tournament controls are all disabled',
    );
  });
}

async function reducedMotionTests() {
  await test('prefers-reduced-motion disables every .pixel-* animation with static cues', () => {
    const css = readFileSync(new URL('../media/chat.css', import.meta.url), 'utf8');
    const match = /@media\s*\(prefers-reduced-motion:\s*reduce\)\s*\{([\s\S]*?)\n\}/.exec(css);
    assert(match, 'chat.css must carry a prefers-reduced-motion block');
    const block = match[1];
    const pixelClasses = [
      'pixel',
      'pixel-running',
      'pixel-paused',
      'pixel-waiting',
      'pixel-blocked',
      'pixel-done',
      'pixel-failed',
      'pixel-cancelled',
    ];
    for (const cls of pixelClasses) {
      assert(
        new RegExp(`\\.${cls}(?![\\w-])`).test(block),
        `reduced-motion block must cover .${cls}`,
      );
    }
    assert(/animation:\s*none/.test(block), 'reduced motion must disable animations');
    assert(/transform:\s*none/.test(block), 'reduced motion must disable transforms');
    for (const cls of pixelClasses.filter((entry) => entry !== 'pixel')) {
      assert(
        new RegExp(`\\.${cls}(?![\\w-])\\s*\\{[^}]*opacity:`).test(block),
        `reduced motion must keep a static opacity cue for .${cls}`,
      );
    }
  });
}

// ------------------------------------------------------- packaged VSIX layout

/** Assert the packaged layout is the Faktor-owned panel, self-contained. */
async function packagedLayoutTests(dir) {
  await test(`packaged VSIX layout is self-contained (${dir})`, () => {
    assert(existsSync(dir), `packaged dir does not exist: ${dir}`);
    for (const rel of [
      'out/extension.js',
      'out/webview.js',
      'out/daemon.js',
      'out/steer.js',
      'media/chat.js',
      'media/chat.css',
      'media/composer-state.js',
      'media/faktor.svg',
    ]) {
      assert(existsSync(join(dir, ...rel.split('/'))), `packaged extension is missing ${rel}`);
    }
    // The Faktor-owned panel ships exactly the hand-written media files: an
    // allowlist makes ANY extra file (a vendored closure included) a failure.
    const media = readdirSync(join(dir, 'media')).sort();
    assertDeepEqual(media, ['chat.css', 'chat.js', 'composer-state.js', 'faktor.svg'], 'media/ ships exactly the Faktor-owned panel');
    // out/ ships exactly one compiled module per src/ module: an extra
    // bridge/closure artifact is a failure even when nobody names it.
    const sources = readdirSync(new URL('../src', import.meta.url))
      .filter((name) => name.endsWith('.ts'))
      .map((name) => name.replace(/\.ts$/, '.js'))
      .sort();
    const compiled = readdirSync(join(dir, 'out'))
      .filter((name) => name.endsWith('.js'))
      .sort();
    assertDeepEqual(compiled, sources, 'out/ ships exactly the compiled src/ modules');
    const built = readFileSync(join(dir, 'out', 'webview.js'), 'utf8');
    assert(
      built.includes("'media'") && built.includes("'chat.js'"),
      'compiled webview.js must resolve the Faktor-owned media/chat.js',
    );
    assert(
      !built.includes('FAKTOR_UI_BUNDLE') && !/'\.\.',\s*'\.\.'/.test(built),
      'compiled webview.js must not reference a bundle override or checkout escape',
    );
    for (const rel of ['media/chat.js', 'media/composer-state.js', 'media/chat.css']) {
      const text = readFileSync(join(dir, ...rel.split('/')), 'utf8');
      assert(text.length > 0, `packaged ${rel} must not be empty`);
    }
  });
  await test(`packaged panel scripts carry a strict nonce-only CSP (${dir})`, () => {
    const built = readFileSync(join(dir, 'out', 'webview.js'), 'utf8');
    for (const marker of [
      "default-src 'none'",
      "script-src 'nonce-",
      "connect-src 'none'",
    ]) {
      assert(built.includes(marker), `compiled webview.js must keep the CSP marker ${marker}`);
    }
    assert(!/https?:\/\//.test(built.replace(/http:\/\/127\.0\.0\.1/g, '')), 'no remote script sources');
  });
}

// -------------------------------------------------------------------- main

function taskProofJson(overrides = {}) {
  const hexOid = (fill) => fill.repeat(40);
  const step = (name, status, detail) => ({
    step: name,
    status,
    present: status !== 'missing',
    detail,
    snapshot: null,
    seq: 2,
    atMs: 1700000000000,
  });
  return {
    schema: 'faktor-task-proof/v1',
    sessionId: '7',
    taskId: '3',
    proofState: 'verified',
    proofStateReason: null,
    task: {
      state: 'verified_complete',
      revision: '4',
      goal: 'ship it',
      updatedMs: 1700000000000,
      acceptanceCriteria: ['build passes'],
    },
    criteria: {
      recordId: '1',
      recordStatus: 'passed',
      recordRevision: '3',
      certifiesCompletion: true,
      total: 2,
      passed: 2,
      failed: 0,
      unavailable: 0,
      items: [],
      truncated: false,
    },
    checks: {
      total: 3,
      passed: 3,
      failed: 0,
      other: 0,
      requiredTotal: 2,
      requiredPassed: 2,
      requiredFailed: 0,
      requiredAllPassed: true,
      items: [],
      truncated: false,
    },
    review: {
      recordId: '1',
      status: 'passed',
      reviewer: { id: 'reviewer-1' },
      independentVerdict: 'passed',
      independentCriteria: ['c2'],
    },
    trees: {
      runBase: 'tm1:aaa',
      verified: 'tm1:bbb',
      landed: 'tm1:bbb',
      landedEqualsVerified: true,
      sourceCount: 1,
    },
    publication: {
      verificationRecord: '1',
      gitTreeOid: hexOid('1'),
      commitOid: hexOid('2'),
      localRef: 'refs/heads/main',
      remoteRef: `origin:refs/heads/main@${hexOid('2')}`,
      remoteHeadOid: hexOid('2'),
      pullRequest: {
        provider: 'github',
        operationKey: 'task:3:rev:3:github:pull_request',
        id: 'pr-42',
        version: `${hexOid('2')}@1700000000`,
        headOid: hexOid('2'),
        state: 'completed',
      },
    },
    cost: {
      status: 'known',
      reason: null,
      spentCostMicro: 12,
      maxCostMicro: 1000000,
      openReservedMicro: 0,
      openReservations: 0,
      uncertainReservedMicro: 0,
      uncertainReservations: 0,
      settledCount: 1,
    },
    completion: {
      contract: {
        includeCommit: false,
        includePush: true,
        includePr: true,
        revision: '3',
        requestedSteps: ['push', 'pr'],
      },
      steps: [step('push', 'succeeded', 'pushed to origin'), step('pr', 'succeeded', 'opened pr-42')],
      records: [step('push', 'succeeded', 'pushed to origin'), step('pr', 'succeeded', 'opened pr-42')],
      gate: { status: 'satisfied', reason: null },
      truncated: false,
    },
    integration: {
      present: true,
      inFlight: false,
      runId: 'run-1',
      finalRoot: '/tmp/root',
      finalSnapshotHash: 'tm1:bbb',
      runBaseSnapshot: 'tm1:aaa',
      candidateSnapshot: 'tm1:bbb',
      landedSnapshot: 'tm1:bbb',
      integratedFileCount: 1,
      conflictCount: 0,
      sourceCount: 1,
      proofBasisDigest: null,
      atMs: 5,
      txn: {
        txnId: 'blake3:x',
        phase: 'landed',
        ownerRoot: '/tmp/o',
        candidateRoot: '/tmp/c',
        pathCount: 0,
        appliedCount: 0,
        conflicts: [],
        atMs: 6,
      },
    },
    unavailable: [],
    ...overrides,
  };
}

function proofCockpitWith(payload, proofUnavailable = null) {
  const taskView = nc.validateTaskViews([
    {
      ...clone(taskViewJson),
      acceptance_criteria: ['build passes'],
    },
  ])[0];
  return cp.buildCockpit({
    task: {
      goal: taskView.goal,
      state: taskView.state,
      completed: taskView.milestones.completed,
      open: taskView.milestones.open,
      testsRun: taskView.tests.run,
      testsFailed: taskView.tests.failed,
      changedFiles: taskView.changedFiles,
      budget: taskView.budget,
      acceptanceCriteria: taskView.acceptanceCriteria,
      plan: taskView.plan,
      blockers: taskView.blockers.map((blocker) => blocker.detail),
      evidenceRefs: taskView.evidenceRefs,
      phase: taskView.phase,
      progress: taskView.progress,
    },
    agents: [],
    verification: null,
    usage: null,
    taskVerification: null,
    proof: payload,
    proofUnavailable,
  });
}

async function proofSummaryTests() {
  await test('strict proof validator accepts the served payload and refuses unknown verdicts', () => {
    const proof = nc.validateTaskProof(taskProofJson());
    assertEqual(proof.proofState, 'verified');
    assertEqual(proof.criteria.passed, 2);
    assertEqual(proof.checks.requiredPassed, 2);
    assertEqual(proof.publication.pullRequest.id, 'pr-42');
    assertEqual(proof.publication.remoteHeadOid, '2'.repeat(40));
    assertEqual(proof.cost.spentCostMicro, 12n);
    assertEqual(proof.completion.steps.length, 2);
    assertEqual(proof.trees.landedEqualsVerified, true);
    // A foreign schema or an unexplained verdict is a loud refusal (never a
    // rendered VERIFIED).
    assertProtocol(
      () => nc.validateTaskProof(taskProofJson({ schema: 'faktor-task-proof/v2' })),
      'unsupported proof schema',
    );
    assertProtocol(
      () => nc.validateTaskProof(taskProofJson({ proofState: 'complete' })),
      'unknown proof verdict',
    );
    assertProtocol(
      () => nc.validateTaskProof(taskProofJson({ cost: 'cheap' })),
      'expected an object',
    );
    const missing = taskProofJson();
    delete missing.review;
    assertProtocol(() => nc.validateTaskProof(missing), 'missing required field review');
    assertProtocol(
      () => nc.validateTaskProof(taskProofJson({ checks: { total: 1 } })),
      'missing required field',
    );
  });

  await test('cockpit renders the daemon VERIFIED view with drill-down, never fabricating one', () => {
    const verified = proofCockpitWith(nc.validateTaskProof(taskProofJson()));
    assert(verified.proof, 'the cockpit carries the proof view');
    assertEqual(verified.proof.state, 'verified');
    const sections = cp.cockpitSections(verified);
    const proof = sections.find((section) => section.key === 'proof');
    assert(proof.present, 'the proof section renders');
    const summary = proof.lines[0];
    assert(summary.startsWith('VERIFIED'), summary);
    assert(summary.includes('criteria 2/2 passed'), summary);
    assert(summary.includes('checks 3/3 passed (required 2/2)'), summary);
    assert(summary.includes('review passed'), summary);
    assert(summary.includes('verified==landed: yes'), summary);
    assert(summary.includes('commit 222222222222'), summary);
    assert(summary.includes('remote head 222222222222'), summary);
    assert(summary.includes('PR pr-42'), summary);
    assert(summary.includes('spend 12\u00b5$ of 1000000\u00b5$'), summary);
    // Drill-down: every durable step + the gate verdict.
    assert(proof.lines.some((line) => line === '[succeeded] push \u2014 pushed to origin'), JSON.stringify(proof.lines));
    assert(proof.lines.some((line) => line === '[succeeded] pr \u2014 opened pr-42'), JSON.stringify(proof.lines));
    assert(proof.lines.some((line) => line === 'gate: satisfied'), JSON.stringify(proof.lines));

    // An `unavailable` daemon verdict renders explicitly unavailable — and the
    // section NEVER contains the word VERIFIED.
    const unavailable = proofCockpitWith(
      nc.validateTaskProof(
        taskProofJson({
          proofState: 'unavailable',
          proofStateReason: 'landed_snapshot mismatch: the landed integration snapshot does not equal the verified snapshot',
          unavailable: [
            {
              component: 'landed_snapshot',
              kind: 'mismatch',
              reason: 'the landed integration snapshot does not equal the verified snapshot',
            },
          ],
        }),
      ),
    );
    assertEqual(unavailable.proof.state, 'unavailable');
    const unavailableLines = cp
      .cockpitSections(unavailable)
      .find((section) => section.key === 'proof').lines;
    assert(unavailableLines[0].startsWith('VERIFICATION UNAVAILABLE'), unavailableLines[0]);
    assert(
      unavailableLines.some((line) => line.includes('landed_snapshot (mismatch)')),
      JSON.stringify(unavailableLines),
    );
    assert(
      !unavailableLines.some((line) => /(^|\W)VERIFIED(\W|$)/.test(line)),
      `an unavailable proof must never render VERIFIED: ${JSON.stringify(unavailableLines)}`,
    );

    // A failed fetch (typed protocol refusal) is the same explicit
    // unavailable state — the extension never shows a stale VERIFIED.
    const refused = proofCockpitWith(null, 'GET /native/tasks/{id}/proof: corrupt_durable_state');
    assertEqual(refused.proof.state, 'unavailable');
    const refusedLines = cp
      .cockpitSections(refused)
      .find((section) => section.key === 'proof').lines;
    assert(refusedLines[0].startsWith('VERIFICATION UNAVAILABLE'), refusedLines[0]);
    assert(
      refusedLines.some((line) => line.includes('corrupt_durable_state')),
      JSON.stringify(refusedLines),
    );
    assert(
      !refusedLines.some((line) => /(^|\W)VERIFIED(\W|$)/.test(line)),
      JSON.stringify(refusedLines),
    );

    // An unverified task says NOT VERIFIED; an unavailable cost read is
    // honest inside a verified story (the proof itself is unaffected).
    const unverified = proofCockpitWith(
      nc.validateTaskProof(
        taskProofJson({
          proofState: 'unverified',
          criteria: { ...taskProofJson().criteria, passed: 1, failed: 1 },
          cost: { ...taskProofJson().cost, status: 'unavailable', reason: 'budget read pool', spentCostMicro: null, maxCostMicro: null },
        }),
      ),
    );
    const unverifiedLines = cp
      .cockpitSections(unverified)
      .find((section) => section.key === 'proof').lines;
    assert(unverifiedLines[0].startsWith('NOT VERIFIED'), unverifiedLines[0]);
    assert(unverifiedLines[0].includes('criteria 1/2 passed'), unverifiedLines[0]);
    assert(unverifiedLines[0].includes('spend unavailable (budget read pool)'), unverifiedLines[0]);
    assert(
      !unverifiedLines.some((line) => line.startsWith('VERIFIED')),
      JSON.stringify(unverifiedLines),
    );
  });
}

// ------------------------------------------------ usage / credits panel (v1)

async function usagePanelTests() {
  const admin = nc.validateIdentity(clone(identityJson)).identity;
  const member = nc.validateIdentity(clone(memberIdentityJson)).identity;
  const entitlements = nc.validateEntitlements(clone(entitlementsJson)).entitlements;
  const usage = nc.validateBillingUsage(clone(billingUsageJson));
  const panelInput = (overrides) => ({
    identity: admin,
    entitlements,
    usage,
    refusal: null,
    cursor: null,
    hasPrev: false,
    ...overrides,
  });

  await test('usage panel renders populated aggregates, credits and the exact limits', () => {
    const panel = cp.buildUsagePanel(panelInput({}));
    assertEqual(panel.state, 'ok');
    assertEqual(panel.reason, null);
    assertEqual(panel.organization, 'org-local');
    assertEqual(panel.planId, 'pro');
    assertEqual(panel.planFound, true);
    assertEqual(panel.period.totalTokens, 1775);
    assertEqual(panel.period.inputTokens, 1000);
    assertEqual(panel.period.reasoningTokens, 25);
    assertEqual(panel.period.managedCostMicro, '700000');
    assertEqual(panel.period.byokCostMicro, '200000');
    assertEqual(panel.period.providerCostMicro, '900000');
    assertEqual(panel.period.correctedEvents, 1);
    assertEqual(panel.period.tasks[0].taskId, 3);
    assertEqual(panel.period.tasks[0].runId, 'r1');
    assertEqual(panel.period.tasks[0].totals.inputTokens, 400);
    assertEqual(panel.credits.balanceMicro, '3100000');
    assertEqual(panel.credits.heldMicro, '250000');
    assertEqual(panel.credits.pendingConsumes, 1);
    assertEqual(panel.subscription.state, 'active');
    assertEqual(panel.inFlight[0].kind, 'integration');
    assertEqual(panel.inFlight[0].endedMs, null);
    assertEqual(panel.page.itemCount, 1);
    assertEqual(panel.page.nextCursor, '9');
    const limits = panel.quotas.map((quota) => quota.limit);
    assert(limits.includes('max_tokens_per_period'), JSON.stringify(limits));
    assert(limits.includes('max_managed_spend_micro_per_period'), JSON.stringify(limits));
    assert(limits.includes('min_credit_balance_micro'), JSON.stringify(limits));
    const tokensQuota = panel.quotas.find((quota) => quota.limit === 'max_tokens_per_period');
    assertEqual(tokensQuota.observed, '1775');
    assertEqual(tokensQuota.value, '100000');
    assertEqual(tokensQuota.exceeded, false);
    const unserved = panel.quotas.find((quota) => quota.limit === 'max_active_tasks');
    assertEqual(unserved.observed, null, 'an unserved observed counter is null, never zero');
    assertEqual(unserved.exceeded, null);
    const lines = cp.usagePanelLines(panel);
    assert(lines.some((line) => line.includes('managed') && line.includes('BYOK')), JSON.stringify(lines));
    assert(
      lines.some((line) => line.includes('quota max_tokens_per_period')),
      JSON.stringify(lines),
    );
    assert(
      lines.some((line) => line.includes('observed not served')),
      JSON.stringify(lines),
    );
    assert(lines.some((line) => line.includes('subscription active')), JSON.stringify(lines));
    assert(lines.some((line) => line.includes('credits balance')), JSON.stringify(lines));
    const section = cp.usagePanelSections(panel)[0];
    assertEqual(section.key, 'usage');
    assertEqual(section.title, 'Usage / Credits');
    assertEqual(section.present, true);
    assertEqual(section.actions.length, 3);
  });

  await test('billing_disabled renders "billing disabled locally", never zeros', () => {
    const panel = cp.buildUsagePanel(
      panelInput({
        entitlements: null,
        usage: null,
        refusal: {
          code: 'billing_disabled',
          reason: '409 billing_disabled: commercial billing is disabled (enable the [billing] section to use it)',
        },
      }),
    );
    assertEqual(panel.state, 'disabled');
    assertEqual(panel.period, null);
    assertEqual(panel.credits, null);
    assertDeepEqual(panel.quotas, []);
    const lines = cp.usagePanelLines(panel);
    assert(lines[0].startsWith('billing disabled locally'), lines[0]);
    assert(lines[0].includes('billing_disabled'), lines[0]);
    assert(!lines.some((line) => line.includes('\u00b5$')), `no money behind a disabled state: ${JSON.stringify(lines)}`);
    assert(!lines.some((line) => line.includes('quota')), JSON.stringify(lines));
    assert(!lines.some((line) => line.includes('tokens')), JSON.stringify(lines));
    assertEqual(cp.usagePanelSections(panel)[0].present, false);
  });

  await test('expired / canceled / lapsed subscriptions render explicit banners', () => {
    const expired = cp.buildUsagePanel(
      panelInput({
        identity: member,
        entitlements: {
          ...clone(entitlements),
          subscription_status: 'expired',
          subscription_active: false,
          subscription_expires_ms: 1_700_000_000_000,
        },
      }),
    );
    assertEqual(expired.subscription.state, 'expired');
    assertEqual(expired.subscription.active, false);
    const expiredLines = cp.usagePanelLines(expired);
    assert(expiredLines.some((line) => line.startsWith('[EXPIRED]')), JSON.stringify(expiredLines));
    assert(
      expiredLines.some((line) => line.startsWith('[EXPIRED]') && line.includes('new tasks are denied')),
      JSON.stringify(expiredLines),
    );
    const grace = cp.buildUsagePanel(
      panelInput({
        entitlements: { ...clone(entitlements), subscription_status: 'active', subscription_active: false },
      }),
    );
    assertEqual(grace.subscription.state, 'grace', 'a durable active row that is effectively inactive is a lapse');
    const graceLines = cp.usagePanelLines(grace);
    assert(graceLines.some((line) => line.startsWith('[GRACE]')), JSON.stringify(graceLines));
    assert(
      graceLines.some((line) => line.startsWith('[GRACE]') && line.includes('inactive')),
      JSON.stringify(graceLines),
    );
    const canceled = cp.buildUsagePanel(
      panelInput({
        entitlements: { ...clone(entitlements), subscription_status: 'canceled', subscription_active: false },
      }),
    );
    assertEqual(canceled.subscription.state, 'canceled');
    assert(
      cp.usagePanelLines(canceled).some((line) => line.startsWith('[CANCELED]')),
      JSON.stringify(cp.usagePanelLines(canceled)),
    );
    const none = cp.buildUsagePanel(
      panelInput({ entitlements: { ...clone(entitlements), subscription_status: null, subscription_active: false } }),
    );
    assertEqual(none.subscription.state, 'unavailable');
  });

  await test('quota-exceeded styling names the exact breached limit', () => {
    const over = nc.validateEntitlements({
      ...clone(entitlementsJson),
      entitlements: {
        ...clone(entitlementsJson.entitlements),
        total_tokens: 250_000,
        managed_spend_micro: 1_000_000,
      },
    }).entitlements;
    const panel = cp.buildUsagePanel(panelInput({ entitlements: over }));
    const exceeded = panel.quotas.filter((quota) => quota.exceeded === true).map((quota) => quota.limit);
    assert(exceeded.includes('max_tokens_per_period'), JSON.stringify(exceeded));
    assert(
      exceeded.includes('max_managed_spend_micro_per_period'),
      JSON.stringify(exceeded),
    );
    const lines = cp.usagePanelLines(panel);
    const tokenLine = lines.find((line) => line.includes('max_tokens_per_period'));
    assert(tokenLine.startsWith('[EXCEEDED] quota max_tokens_per_period'), tokenLine);
    assert(tokenLine.includes('/ limit 100000'), tokenLine);
    const moneyLine = lines.find((line) => line.includes('max_managed_spend_micro_per_period'));
    assert(moneyLine.startsWith('[EXCEEDED]'), moneyLine);
    assertEqual(lines.filter((line) => line.startsWith('[EXCEEDED]')).length, 2);
    // The floor limit (min credit balance) is breached BELOW its value.
    const low = nc.validateEntitlements({
      ...clone(entitlementsJson),
      entitlements: {
        ...clone(entitlementsJson.entitlements),
        credits: { ...clone(creditBalanceJson), granted_micro: 100, consumed_micro: 50, refunded_micro: 0 },
      },
    }).entitlements;
    const floorPanel = cp.buildUsagePanel(panelInput({ entitlements: low }));
    const floorQuota = floorPanel.quotas.find((quota) => quota.limit === 'min_credit_balance_micro');
    assertEqual(floorQuota.exceeded, true);
    assert(
      cp.usagePanelLines(floorPanel).some((line) => line.startsWith('[EXCEEDED] quota min_credit_balance_micro')),
      JSON.stringify(cp.usagePanelLines(floorPanel)),
    );
  });

  await test('grant-credits affordance follows the credits_grant capability', () => {
    const adminPanel = cp.buildUsagePanel(panelInput({}));
    assertEqual(adminPanel.canGrantCredits, true);
    assertEqual(adminPanel.actions.find((action) => action.key === 'grant-credits').enabled, true);
    assertEqual(adminPanel.grantDisabledReason, null);
    assert(
      cp.usagePanelLines(adminPanel).some((line) => line.includes('grants credits')),
      JSON.stringify(cp.usagePanelLines(adminPanel)),
    );
    const memberPanel = cp.buildUsagePanel(panelInput({ identity: member }));
    assertEqual(memberPanel.canGrantCredits, false);
    assertEqual(memberPanel.actions.find((action) => action.key === 'grant-credits').enabled, false);
    assert(
      memberPanel.grantDisabledReason.includes('member') &&
        memberPanel.grantDisabledReason.includes('credits_grant'),
      memberPanel.grantDisabledReason,
    );
    assert(
      cp.usagePanelLines(memberPanel).some((line) => line.includes('grant credits disabled')),
      JSON.stringify(cp.usagePanelLines(memberPanel)),
    );
    const anonymous = cp.buildUsagePanel(panelInput({ identity: null }));
    assertEqual(anonymous.canGrantCredits, false);
    assertEqual(anonymous.actions.find((action) => action.key === 'grant-credits').enabled, false);
    assert(
      anonymous.grantDisabledReason.includes('identity'),
      anonymous.grantDisabledReason,
    );
  });

  await test('cursor pagination controls wire next/prev from the served cursors', () => {
    const first = cp.buildUsagePanel(panelInput({}));
    assertEqual(first.page.cursor, null);
    assertEqual(first.page.nextCursor, '9');
    assertEqual(first.page.hasPrev, false);
    assertEqual(first.actions.find((action) => action.key === 'usage-next').enabled, true);
    assertEqual(first.actions.find((action) => action.key === 'usage-prev').enabled, false);
    const second = cp.buildUsagePanel(
      panelInput({
        usage: { ...clone(billingUsageJson), nextCursor: null },
        cursor: '9',
        hasPrev: true,
      }),
    );
    assertEqual(second.page.cursor, '9');
    assertEqual(second.page.nextCursor, null);
    assertEqual(second.actions.find((action) => action.key === 'usage-next').enabled, false);
    assertEqual(second.actions.find((action) => action.key === 'usage-prev').enabled, true);
    const lines = cp.usagePanelLines(second);
    assert(lines.some((line) => line.includes('cursor 9') && line.includes('next none')), JSON.stringify(lines));
  });

  await test('malformed or refused payloads render honest unavailable states', () => {
    assertProtocol(
      () =>
        nc.validateBillingUsage({
          ...clone(billingUsageJson),
          fold: { ...clone(billingUsageJson.fold), totals: { ...clone(usageBucketsJson), input_tokens: 'many' } },
        }),
      'expected a finite number',
    );
    assertProtocol(
      () => nc.validateEntitlements({ ...clone(entitlementsJson), entitlements: { ...clone(entitlementsJson.entitlements), credits: null } }),
      'expected an object',
    );
    const panel = cp.buildUsagePanel(
      panelInput({
        entitlements: null,
        usage: null,
        refusal: { code: 'malformed', reason: 'GET /native/usage: missing required field credits' },
      }),
    );
    assertEqual(panel.state, 'unavailable');
    assertEqual(panel.period, null);
    assertEqual(panel.credits, null);
    assertDeepEqual(panel.quotas, []);
    const lines = cp.usagePanelLines(panel);
    assert(lines[0].startsWith('usage unavailable'), lines[0]);
    assert(lines[0].includes('missing required field credits'), lines[0]);
    assert(!lines.some((line) => line.includes('\u00b5$')), `no fabricated money: ${JSON.stringify(lines)}`);
    assert(!lines.some((line) => line.includes('quota')), JSON.stringify(lines));
    assertEqual(cp.usagePanelSections(panel)[0].present, false);
  });
}

// ------------------------------------------- control-plane credential store
//
// Fake SecretStorage rows + a fake plaintext setting: store/retrieve,
// one-shot migration (delete the plaintext, store the secret), the refusal
// path (a declined migration never sends the plaintext) and logout (remote
// revoke attempted, local secret deleted unconditionally).

class FakeSecretStorage {
  constructor(rows = new Map()) {
    this.rows = rows;
    this.operations = [];
    this.changeListeners = [];
  }

  async get(key) {
    this.operations.push(`get:${key}`);
    return this.rows.get(key);
  }

  async store(key, value) {
    this.operations.push(`store:${key}`);
    this.rows.set(key, value);
  }

  async delete(key) {
    this.operations.push(`delete:${key}`);
    this.rows.delete(key);
  }

  /** The structural `vscode.SecretStorage.onDidChange` seam. */
  onDidChange(listener) {
    this.changeListeners.push(listener);
    return {
      dispose: () => {
        this.changeListeners = this.changeListeners.filter((entry) => entry !== listener);
      },
    };
  }

  /** An EXTERNAL writer (another window / the keychain UI). */
  async externalStore(key, value) {
    this.rows.set(key, value);
    this.emitChange(key);
  }

  async externalDelete(key) {
    this.rows.delete(key);
    this.emitChange(key);
  }

  emitChange(key) {
    for (const listener of this.changeListeners.slice()) {
      listener({ key });
    }
  }
}

/** Lets chained microtask applies settle before assertions. */
function settle() {
  return new Promise((resolve) => setImmediate(resolve));
}

class FakePlaintextSetting {
  constructor(value = null) {
    this.value = value;
    this.clears = 0;
  }

  read() {
    return this.value;
  }

  async clear() {
    this.clears += 1;
    this.value = null;
  }
}

const controlPlaneScope = { endpoint: 'https://CP.Example/ ', organization: ' org-1 ' };

async function controlPlaneCredentialTests() {
  const expectedKey = 'faktor.controlPlaneToken.v1.https%3A%2F%2Fcp.example.org-1';

  await test('the secret key is the endpoint plus organization, never the token', () => {
    assertEqual(cpa.controlPlaneSecretKey(controlPlaneScope), expectedKey);
    assert(cpa.controlPlaneScopeValid(controlPlaneScope), 'a valid scope must be recognized');
    assert(!cpa.controlPlaneScopeValid(null), 'null scope is invalid');
    assert(!cpa.controlPlaneScopeValid({ endpoint: 'https://x', organization: '   ' }), 'blank org invalid');
    let threw = false;
    try {
      cpa.controlPlaneSecretKey({ endpoint: '', organization: 'o' });
    } catch {
      threw = true;
    }
    assert(threw, 'an incomplete scope must refuse to mint a secret key');
  });

  await test('SecretStorage wins and a leftover plaintext copy is deleted, never kept alongside', async () => {
    const secrets = new FakeSecretStorage(new Map([[expectedKey, 'stored-token']]));
    const plaintext = new FakePlaintextSetting('legacy-token');
    let confirmCalls = 0;
    const resolution = await cpa.resolveControlPlaneToken({
      secrets,
      plaintext,
      scope: controlPlaneScope,
      legacyToken: plaintext.read(),
      confirmMigration: async () => {
        confirmCalls += 1;
        return true;
      },
    });
    assertEqual(resolution.kind, 'secret');
    assertEqual(resolution.token, 'stored-token');
    assertEqual(resolution.legacyCleared, true, 'the leftover plaintext copy must be deleted');
    assertEqual(plaintext.value, null);
    assertEqual(confirmCalls, 0, 'no migration prompt when the secret already exists');
  });

  await test('a plaintext credential migrates once: secret stored, setting deleted', async () => {
    const secrets = new FakeSecretStorage();
    const plaintext = new FakePlaintextSetting('cp-legacy-value');
    const resolution = await cpa.resolveControlPlaneToken({
      secrets,
      plaintext,
      scope: controlPlaneScope,
      legacyToken: plaintext.read(),
      confirmMigration: async () => true,
    });
    assertEqual(resolution.kind, 'migrated');
    assertEqual(resolution.token, 'cp-legacy-value');
    assertEqual(secrets.rows.get(expectedKey), 'cp-legacy-value');
    assertEqual(plaintext.value, null, 'the plaintext setting must be deleted after migration');
    assertEqual(plaintext.clears, 1, 'exactly one delete call');
    assertEqual(cpa.controlPlaneTokenForClient(resolution), 'cp-legacy-value');
    // A second resolution reads only the secret (no plaintext, no prompt).
    let prompts = 0;
    const again = await cpa.resolveControlPlaneToken({
      secrets,
      plaintext,
      scope: controlPlaneScope,
      legacyToken: plaintext.read(),
      confirmMigration: async () => {
        prompts += 1;
        return true;
      },
    });
    assertEqual(again.kind, 'secret');
    assertEqual(prompts, 0);
  });

  await test('a declined migration refuses the plaintext for cloud calls', async () => {
    const secrets = new FakeSecretStorage();
    const plaintext = new FakePlaintextSetting('cp-refused-value');
    const resolution = await cpa.resolveControlPlaneToken({
      secrets,
      plaintext,
      scope: controlPlaneScope,
      legacyToken: plaintext.read(),
      confirmMigration: async () => false,
    });
    assertEqual(resolution.kind, 'legacy-refused');
    assertEqual(cpa.controlPlaneTokenForClient(resolution), null, 'a refused plaintext is never returned');
    assertEqual(secrets.rows.size, 0, 'a declined migration stores nothing');
    assertEqual(plaintext.value, 'cp-refused-value', 'the operator keeps (and can remove) the setting');
    assert(
      !JSON.stringify(resolution).includes('cp-refused-value'),
      'the refusal may never carry the plaintext value',
    );
    // The client built from the refusal sends no header at all.
    const routes = { 'GET /native/identity': () => jsonResponse(identityJson) };
    const refused = makeClient(routes, {
      controlToken: cpa.controlPlaneTokenForClient(resolution) ?? undefined,
    });
    await refused.client.identity();
    assertEqual(
      findCall(refused.calls, 'GET', '/native/identity').headers['x-faktor-control-token'],
      undefined,
      'the refused plaintext must never reach a cloud call',
    );
  });

  await test('a plaintext credential without scope coordinates is refused', async () => {
    const secrets = new FakeSecretStorage();
    const plaintext = new FakePlaintextSetting('cp-scopeless-value');
    const resolution = await cpa.resolveControlPlaneToken({
      secrets,
      plaintext,
      scope: null,
      legacyToken: plaintext.read(),
      confirmMigration: async () => true,
    });
    assertEqual(resolution.kind, 'legacy-refused');
    assert(resolution.reason.includes('endpoint'), resolution.reason);
    assertEqual(cpa.controlPlaneTokenForClient(resolution), null);
    assertEqual(secrets.rows.size, 0);
  });

  await test('sign-in stores the secret and deletes the plaintext; blank input is refused', async () => {
    const secrets = new FakeSecretStorage();
    const plaintext = new FakePlaintextSetting('leftover');
    const key = await cpa.storeControlPlaneToken(secrets, plaintext, controlPlaneScope, 'signin-token');
    assertEqual(key, expectedKey);
    assertEqual(secrets.rows.get(expectedKey), 'signin-token');
    assertEqual(plaintext.value, null);
    await assertRejects(
      () => cpa.storeControlPlaneToken(secrets, plaintext, controlPlaneScope, '   '),
      (error) => /empty control-plane credential/.test(error.message),
      'blank credential',
    );
    assertEqual(secrets.rows.size, 1, 'a refused blank credential stores nothing');
  });

  await test('logout sends the real route shape and revokes the exact token; a network error still deletes locally', async () => {
    const secrets = new FakeSecretStorage(new Map([[expectedKey, 'logout-token']]));
    const plaintext = new FakePlaintextSetting('leftover');
    const revoked = [];
    const ok = await cpa.logoutControlPlane({
      secrets,
      plaintext,
      scope: controlPlaneScope,
      session: { organization: 'org-1', sessionId: 'ses-1' },
      revoke: async (token, session) => {
        revoked.push([token, session.organization, session.sessionId]);
      },
    });
    assertDeepEqual(
      revoked,
      [['logout-token', 'org-1', 'ses-1']],
      'the exact stored token and the non-secret session coordinates are revoke inputs',
    );
    assertEqual(ok.remoteRevoked, true);
    assertEqual(ok.hadSecret, true);
    assertEqual(secrets.rows.size, 0, 'logout deletes the local secret');
    assertEqual(plaintext.value, null);
    // Unreachable control plane: the remote failure is reported but never
    // keeps the local credential.
    const failing = new FakeSecretStorage(new Map([[expectedKey, 'token-2']]));
    const failed = await cpa.logoutControlPlane({
      secrets: failing,
      plaintext: new FakePlaintextSetting(null),
      scope: controlPlaneScope,
      session: { organization: 'org-1', sessionId: 'ses-1' },
      revoke: async () => {
        throw new Error('daemon is not running');
      },
    });
    assertEqual(failed.remoteRevoked, false);
    assertEqual(failed.remoteError, 'daemon is not running');
    assertEqual(failing.rows.size, 0, 'a failed remote revoke still deletes the secret');
    // An opaque token without its session id: the route cannot name the
    // session, so nothing is claimed and no revoke call is attempted.
    const unknown = new FakeSecretStorage(new Map([[expectedKey, 'token-3']]));
    let unknownCalls = 0;
    const noSession = await cpa.logoutControlPlane({
      secrets: unknown,
      plaintext: new FakePlaintextSetting(null),
      scope: controlPlaneScope,
      session: null,
      revoke: async () => {
        unknownCalls += 1;
      },
    });
    assertEqual(unknownCalls, 0, 'no session id => no named revoke');
    assertEqual(noSession.remoteRevoked, false);
    assert(noSession.remoteError.includes('auth-session id'), noSession.remoteError);
    assertEqual(unknown.rows.size, 0, 'the impossible revoke still deletes the local secret');
    // No stored credential: no revoke call is attempted.
    const empty = new FakeSecretStorage();
    let calls = 0;
    const none = await cpa.logoutControlPlane({
      secrets: empty,
      plaintext: new FakePlaintextSetting(null),
      scope: controlPlaneScope,
      session: { organization: 'org-1', sessionId: 'ses-1' },
      revoke: async () => {
        calls += 1;
      },
    });
    assertEqual(none.hadSecret, false);
    assertEqual(calls, 0);
  });

  await test('an external rotation during the in-flight revoke is compare-and-deleted, never erased', async () => {
    const secrets = new FakeSecretStorage(new Map([[expectedKey, 'old-token']]));
    const plaintext = new FakePlaintextSetting('leftover');
    const revoked = [];
    const result = await cpa.logoutControlPlane({
      secrets,
      plaintext,
      scope: controlPlaneScope,
      session: { organization: 'org-1', sessionId: 'ses-1' },
      revoke: async (token) => {
        revoked.push(token);
        // The external rotation lands WHILE the revoke is in flight: only the
        // CAPTURED token may be revoked, and the NEW secret must survive.
        secrets.rows.set(expectedKey, 'new-token');
      },
    });
    assertDeepEqual(revoked, ['old-token'], 'the captured token is revoked, never the rotated one');
    assertEqual(result.rotatedDuringLogout, true, 'the rotation is reported typed');
    assertEqual(result.remoteRevoked, true);
    assertEqual(secrets.rows.get(expectedKey), 'new-token', 'the new secret is left untouched');
    assertEqual(plaintext.value, null, 'the plaintext copy is still cleared');

    // A rotation landing after a FAILED revoke leaves the new credential too,
    // and both outcomes are reported.
    const failing = new FakeSecretStorage(new Map([[expectedKey, 'old-token']]));
    const failed = await cpa.logoutControlPlane({
      secrets: failing,
      plaintext: new FakePlaintextSetting(null),
      scope: controlPlaneScope,
      session: { organization: 'org-1', sessionId: 'ses-1' },
      revoke: async () => {
        failing.rows.set(expectedKey, 'newer-token');
        throw new Error('network down');
      },
    });
    assertEqual(failed.rotatedDuringLogout, true);
    assertEqual(failed.remoteError, 'network down');
    assertEqual(failing.rows.get(expectedKey), 'newer-token');
    // The no-rotation path still deletes exactly the captured credential.
    const plain = new FakeSecretStorage(new Map([[expectedKey, 'plain-token']]));
    const removed = await cpa.logoutControlPlane({
      secrets: plain,
      plaintext: new FakePlaintextSetting(null),
      scope: controlPlaneScope,
      session: { organization: 'org-1', sessionId: 'ses-1' },
      revoke: async () => {},
    });
    assertEqual(removed.rotatedDuringLogout, false);
    assertEqual(plain.rows.size, 0, 'an unrotated sign-out still deletes locally');
  });

  await test('the client sends the credentialed header only after a secure resolution', async () => {
    const routes = {
      'GET /native/identity': () => jsonResponse(identityJson),
      'POST /native/sso/logout': () => jsonResponse({ ok: true }),
    };
    const { client, calls } = makeClient(routes);
    await client.identity();
    assertEqual(
      findCall(calls, 'GET', '/native/identity').headers['x-faktor-control-token'],
      undefined,
      'no credential => no control-plane header',
    );
    client.setControlToken('live-token');
    await client.identity();
    assertEqual(
      calls.filter((call) => call.path === '/native/identity').at(-1).headers['x-faktor-control-token'],
      'live-token',
    );
    await client.revokeControlSession('org-1', 'ses-1');
    const logout = findCall(calls, 'POST', '/native/sso/logout');
    assertEqual(logout.headers['x-faktor-control-token'], 'live-token');
    assertEqual(logout.headers.Authorization, 'Bearer selftest-token', 'the daemon password rides Authorization');
    assertEqual(logout.body.organization, 'org-1');
    assertEqual(logout.body.session_id, 'ses-1');
    client.setControlToken(null);
    await client.identity();
    assertEqual(
      calls.filter((call) => call.path === '/native/identity').at(-1).headers['x-faktor-control-token'],
      undefined,
      'sign-out clears the header',
    );
  });

  await test('revokeControlSession refuses a body the daemon would reject, and a 404 is a typed not-found', async () => {
    const routes = {
      'POST /native/sso/logout': (call) =>
        call.body.session_id === 'ses-missing'
          ? jsonResponse(
              { error: { code: 'not_found', message: 'control-plane session not found', retryable: false } },
              404,
            )
          : jsonResponse({ ok: true, revoked: true, alreadyRevoked: false }),
    };
    const { client } = makeClient(routes, { controlToken: 'live-token' });
    await assertRejects(
      () => client.revokeControlSession('  ', 'ses-1'),
      (error) => /organization/.test(error.message),
      'blank organization',
    );
    await assertRejects(
      () => client.revokeControlSession('org-1', ''),
      (error) => /auth-session id/.test(error.message),
      'blank session id',
    );
    await assertRejects(
      () => client.revokeControlSession('org-1', 'ses-missing'),
      (error) => error.status === 404 && error.code === 'not_found',
      'the route 404 is the typed not-found, never a route-absent story',
    );
    // The success path resolves without a body the daemon would refuse.
    await client.revokeControlSession('org-1', 'ses-1');
  });

  await test('an external SecretStorage change reaches the running client; removal clears the old token', async () => {
    const secrets = new FakeSecretStorage(new Map([[expectedKey, 'first-token']]));
    const plaintext = new FakePlaintextSetting(null);
    const applied = [];
    const target = { setControlToken: (value) => applied.push(value) };
    const errors = [];
    const watch = cpa.watchControlPlaneSecretChanges({
      secrets,
      apply: async () => {
        const resolution = await cpa.resolveControlPlaneToken({
          secrets,
          plaintext,
          scope: controlPlaneScope,
          legacyToken: null,
        });
        target.setControlToken(cpa.controlPlaneTokenForClient(resolution));
      },
      onError: (error) => errors.push(error),
    });
    assert(cpa.isControlPlaneSecretKey(expectedKey), 'the key is owned by this module');
    assert(!cpa.isControlPlaneSecretKey('faktor.controlPlaneEndpoint'), 'settings are not secret keys');
    // An EXTERNAL write (another window / keychain UI) replaces the live token.
    await secrets.externalStore(expectedKey, 'second-token');
    await settle();
    assertDeepEqual(applied, ['second-token'], 'the external update reached the client');
    // An EXTERNAL removal clears the old token instead of leaving it live.
    await secrets.externalDelete(expectedKey);
    await settle();
    assertDeepEqual(applied, ['second-token', null], 'the removed secret clears the old token');
    // Unrelated secret keys never apply.
    await secrets.externalStore('faktor.unrelated.secret.row', 'x');
    await settle();
    assertDeepEqual(applied, ['second-token', null], 'only control-plane keys apply');
    // A re-stored credential applies again.
    await secrets.externalStore(expectedKey, 'third-token');
    await settle();
    assertDeepEqual(
      applied,
      ['second-token', null, 'third-token'],
      'the watch keeps applying later changes',
    );
    // Disposal stops applying.
    watch.dispose();
    await secrets.externalStore(expectedKey, 'fourth-token');
    await settle();
    assertDeepEqual(applied, ['second-token', null, 'third-token'], 'a disposed watch stops');
    assertDeepEqual(errors, [], 'no refresh errors on the happy path');
    // A throwing apply surfaces through onError and the watch stays alive.
    const failingWatch = cpa.watchControlPlaneSecretChanges({
      secrets,
      apply: async () => {
        throw new Error('resolution failed');
      },
      onError: (error) => errors.push(error),
    });
    await secrets.externalStore(expectedKey, 'fifth-token');
    await settle();
    assertEqual(errors.length, 1);
    assertEqual(errors[0].message, 'resolution failed');
    failingWatch.dispose();
  });
}

// ------------------------------------------------------- money exactness (v1)
//
// The daemon serializes monetary `*_micro` fields as decimal strings because
// a JavaScript number is only integer-exact to 2^53-1. These rows pin the
// exact parse (string AND legacy number), the loud refusal of an unsafe
// number, exact display and exact bigint aggregation.

const MONEY_I64_MAX = 9223372036854775807n;
const MONEY_SAFE_MAX = 9007199254740991n;

async function moneyTests() {
  const creditsWith = (overrides) => ({
    ok: true,
    organization: 'org-local',
    fold: {
      organization_id: 'org-local',
      totals: usageBucketsJson,
      per_task: [],
      next_cursor: null,
    },
    credits: { ...creditBalanceJson, ...overrides },
    items: [],
    nextCursor: null,
  });

  await test('money parses decimal strings exactly at 0, 2^53-1, 2^53 and i64::MAX', () => {
    assertEqual(mn.microFromDecimal('0'), 0n);
    assertEqual(mn.microFromDecimal('9007199254740991'), MONEY_SAFE_MAX);
    assertEqual(mn.microFromDecimal('9007199254740992'), 9007199254740992n);
    assertEqual(mn.microFromDecimal('9223372036854775807'), MONEY_I64_MAX);
    // End to end through the strict validator: a string money field is exact.
    const parsed = nc.validateBillingUsage(
      creditsWith({
        granted_micro: '9223372036854775807',
        consumed_micro: '9007199254740992',
        refunded_micro: '1',
        held_micro: '0',
      }),
    );
    assertEqual(parsed.credits.granted_micro, MONEY_I64_MAX);
    assertEqual(parsed.credits.consumed_micro, 9007199254740992n);
    // The served u64 domain is exact end to end; hostile strings are
    // refused, never truncated or coerced.
    assertEqual(mn.microFromDecimal('18446744073709551615'), 18446744073709551615n, 'u64::MAX');
    assertEqual(mn.microFromDecimal('9223372036854775808'), 9223372036854775808n);
    assertEqual(mn.microFromDecimal('18446744073709551616'), null, 'above u64::MAX');
    assertEqual(mn.microFromDecimal('-1'), null);
    assertEqual(mn.microFromDecimal('1e3'), null);
    assertEqual(mn.microFromDecimal('1.5'), null);
    assertEqual(mn.microFromDecimal(''), null);
    assertEqual(mn.microFromDecimal(' 1'), null);
    assertProtocol(
      () => nc.validateBillingUsage(creditsWith({ held_micro: '18446744073709551616' })),
      'expected a decimal micro amount string',
    );
    assertProtocol(
      () => nc.validateBillingUsage(creditsWith({ held_micro: '12.5' })),
      'expected a decimal micro amount string',
    );
  });

  await test('money: legacy numbers <= 2^53-1 convert exactly; larger numbers are flagged', () => {
    assertEqual(mn.microFromNumber(0), 0n);
    assertEqual(mn.microFromNumber(1), 1n);
    assertEqual(mn.microFromNumber(Number.MAX_SAFE_INTEGER), MONEY_SAFE_MAX);
    // Above 2^53-1 the number has already lost precision: refuse, never round.
    assertEqual(mn.microFromNumber(Number.MAX_SAFE_INTEGER + 1), null);
    assertEqual(mn.microFromNumber(1.5), null);
    assertEqual(mn.microFromNumber(-1), null);
    assertEqual(mn.microFromNumber(Number.POSITIVE_INFINITY), null);
    // The legacy number form is still accepted on the wire below the flag.
    assertEqual(
      nc.validateBillingUsage(clone(billingUsageJson)).credits.granted_micro,
      5_000_000n,
    );
    assertEqual(
      nc.validateEntitlements(clone(entitlementsJson)).entitlements.limits
        .max_managed_spend_micro_per_period,
      1_000_000n,
    );
    assertProtocol(
      () =>
        nc.validateBillingUsage(
          creditsWith({ granted_micro: Number.MAX_SAFE_INTEGER + 1 }),
        ),
      'not exactly representable',
    );
    assertProtocol(
      () =>
        nc.validateEntitlements({
          ...clone(entitlementsJson),
          entitlements: {
            ...clone(entitlementsJson.entitlements),
            limits: { max_managed_spend_micro_per_period: 2 ** 53 },
          },
        }),
      'not exactly representable',
    );
  });

  await test('money display is exact: no precision loss, no scientific notation', () => {
    assertEqual(mn.microToString(MONEY_I64_MAX), '9223372036854775807');
    assertEqual(mn.microText(MONEY_I64_MAX), '9223372036854775807\u00b5$');
    assertEqual(mn.microText(0n), '0\u00b5$');
    assertEqual(mn.microUsdText(1_234_567n), '1.2346');
    assertEqual(mn.microUsdText(0n), '0.0000');
    assertEqual(mn.microUsdText(MONEY_I64_MAX), '9223372036854.7758');
    assertEqual(mn.microUsdText(1n), '0.0000', 'half-up at the fifth decimal');
    assertEqual(mn.microUsdText(500_000n), '0.5000');
    assert(!mn.microText(MONEY_I64_MAX).includes('e'), 'never scientific notation');
    assert(!mn.microText(MONEY_I64_MAX).includes('E'), 'never scientific notation');
    // The panel renders the exact digits (money line + credits line).
    const panel = cp.buildUsagePanel({
      identity: nc.validateIdentity(clone(identityJson)).identity,
      entitlements: nc.validateEntitlements(bigEntitlementsJson()).entitlements,
      usage: nc.validateBillingUsage(bigBillingUsageJson()),
      refusal: null,
      cursor: null,
      hasPrev: false,
    });
    const lines = cp.usagePanelLines(panel);
    assert(
      lines.some((line) => line.includes('managed 9223372036854775807\u00b5$')),
      JSON.stringify(lines),
    );
    assert(
      lines.some((line) =>
        line.includes('credits balance 9223372036854775807\u00b5$'),
      ),
      JSON.stringify(lines),
    );
  });

  await test('money aggregation is exact in bigint (sums, saturating balance, quota compare)', () => {
    assertEqual(mn.sumMicro([MONEY_SAFE_MAX, 1n]), 9007199254740992n);
    assertEqual(mn.sumMicro([MONEY_I64_MAX - 1n, 1n]), MONEY_I64_MAX);
    assertEqual(mn.sumMicro([]), 0n);
    assertEqual(mn.microBalance(MONEY_I64_MAX, 1n, 1n), MONEY_I64_MAX);
    assertEqual(mn.microBalance(0n, 0n, 5n), 0n, 'the balance saturates at zero');
    const panel = cp.buildUsagePanel({
      identity: nc.validateIdentity(clone(identityJson)).identity,
      entitlements: nc.validateEntitlements(bigEntitlementsJson()).entitlements,
      usage: nc.validateBillingUsage(bigBillingUsageJson()),
      refusal: null,
      cursor: null,
      hasPrev: false,
    });
    assertEqual(panel.credits.balanceMicro, '9223372036854775807');
    assertEqual(panel.period.managedCostMicro, '9223372036854775807');
    // 2^53 vs 2^53+1: a float would collapse the pair and flip the verdict.
    const managed = panel.quotas.find(
      (quota) => quota.limit === 'max_managed_spend_micro_per_period',
    );
    assertEqual(managed.value, '9223372036854775807');
    assertEqual(managed.observed, '9223372036854775806');
    assertEqual(managed.exceeded, false, 'observed below the exact ceiling');
    const floor = panel.quotas.find((quota) => quota.limit === 'min_credit_balance_micro');
    assertEqual(floor.value, '9007199254740993');
    assertEqual(floor.observed, '9223372036854775807');
    assertEqual(floor.exceeded, false, 'the exact balance is above the floor');
    const near = cp.buildUsagePanel({
      identity: nc.validateIdentity(clone(identityJson)).identity,
      entitlements: nc.validateEntitlements(nearBoundaryEntitlementsJson()).entitlements,
      usage: nc.validateBillingUsage(nearBoundaryBillingUsageJson()),
      refusal: null,
      cursor: null,
      hasPrev: false,
    });
    const nearQuota = near.quotas.find(
      (quota) => quota.limit === 'max_managed_spend_micro_per_period',
    );
    assertEqual(nearQuota.observed, '9007199254740992');
    assertEqual(nearQuota.value, '9007199254740993');
    assertEqual(
      nearQuota.exceeded,
      false,
      '2^53 is exactly below 2^53+1 (a float parse would flip this)',
    );
    const over = cp.buildUsagePanel({
      identity: nc.validateIdentity(clone(identityJson)).identity,
      entitlements: nc.validateEntitlements(
        nearBoundaryEntitlementsJson({ managed: '9007199254740993', limit: '9007199254740993' }),
      ).entitlements,
      usage: nc.validateBillingUsage(
        nearBoundaryBillingUsageJson({ managed: '9007199254740993' }),
      ),
      refusal: null,
      cursor: null,
      hasPrev: false,
    });
    const overQuota = over.quotas.find(
      (quota) => quota.limit === 'max_managed_spend_micro_per_period',
    );
    assertEqual(overQuota.exceeded, true, 'exactly at/above the ceiling is exceeded');
  });

  await test('money request projection: number while lossless, else the exact string', () => {
    assertEqual(mn.microWireValue(0n), 0);
    assertEqual(mn.microWireValue(1_000_000n), 1_000_000);
    assertEqual(mn.microWireValue(MONEY_SAFE_MAX), Number(MONEY_SAFE_MAX));
    assertEqual(mn.microWireValue(MONEY_SAFE_MAX + 1n), '9007199254740992');
    assertEqual(mn.microWireValue(MONEY_I64_MAX), '9223372036854775807');
    assertEqual(
      mn.microWireValue(18446744073709551615n),
      '18446744073709551615',
      'the whole served u64 domain projects exactly',
    );
    // A task start with a huge budget sends the exact string, never a rounded
    // number; a small budget stays byte-identical to the legacy number form.
    assertDeepEqual(
      ts.startTaskRequest('goal', { mutationMode: '', maxTokens: 0, maxCostMicro: MONEY_I64_MAX }),
      { goal: 'goal', max_cost_micro: '9223372036854775807' },
    );
    assertDeepEqual(
      ts.startTaskRequest('goal', { mutationMode: '', maxTokens: 0, maxCostMicro: 5n }),
      { goal: 'goal', max_cost_micro: 5 },
    );
  });
}

// Big-money fixtures for the display/aggregation rows above (all strings).
function bigBillingUsageJson() {
  return {
    ok: true,
    organization: 'org-local',
    fold: {
      organization_id: 'org-local',
      totals: {
        ...clone(usageBucketsJson),
        provider_cost_micro: '9223372036854775807',
        managed_cost_micro: '9223372036854775807',
        byok_cost_micro: '0',
      },
      per_task: [],
      next_cursor: null,
    },
    credits: {
      granted_micro: '9223372036854775807',
      consumed_micro: '1',
      refunded_micro: '1',
      held_micro: '0',
      pending_consumes: 0,
    },
    items: [],
    nextCursor: null,
  };
}

function bigEntitlementsJson() {
  return {
    ok: true,
    entitlements: {
      ...clone(entitlementsJson.entitlements),
      credits: clone(bigBillingUsageJson().credits),
      managed_spend_micro: '9223372036854775806',
      limits: {
        max_tokens_per_period: '100000',
        max_managed_spend_micro_per_period: '9223372036854775807',
        min_credit_balance_micro: '9007199254740993',
        max_active_tasks: '4',
      },
    },
  };
}

function nearBoundaryBillingUsageJson(overrides = {}) {
  const managed = overrides.managed ?? '9007199254740992';
  return {
    ...clone(bigBillingUsageJson()),
    fold: {
      organization_id: 'org-local',
      totals: {
        ...clone(usageBucketsJson),
        managed_cost_micro: managed,
        provider_cost_micro: managed,
      },
      per_task: [],
      next_cursor: null,
    },
    credits: {
      granted_micro: managed,
      consumed_micro: '0',
      refunded_micro: '0',
      held_micro: '0',
      pending_consumes: 0,
    },
  };
}

function nearBoundaryEntitlementsJson(overrides = {}) {
  const limit = overrides.limit ?? '9007199254740993';
  return {
    ok: true,
    entitlements: {
      ...clone(entitlementsJson.entitlements),
      credits: clone(nearBoundaryBillingUsageJson(overrides).credits),
      managed_spend_micro: overrides.managed ?? '9007199254740992',
      limits: {
        max_managed_spend_micro_per_period: limit,
        min_credit_balance_micro: '0',
      },
    },
  };
}

async function main() {
  await validatorAccepts();
  await validatorRejects();
  await clientAccepts();
  await clientRejects();
  await eventStreamTests();
  await stateTests();
  await daemonTests();
  await shadowDefaultTests();
  await completionContractTests();
  await pendingSubmissionTests();
  await boardAndForwardingTests();
  await draftPreservationTests();
  await runStateTests();
  await workspaceBindingTests();
  await childInspectionTests();
  await pixelAgentTests();
  await cockpitTests();
  await acceptanceProofTests();
  await proofSummaryTests();
  await usagePanelTests();
  await moneyTests();
  await controlPlaneCredentialTests();
  await presentationWebviewTests();
  await tournamentWebviewTests();
  await reducedMotionTests();
  if (packagedDir !== null && packagedDir !== undefined) {
    await packagedLayoutTests(packagedDir);
  }

  console.log(`\n${passed} passed, ${failed} failed`);
  if (failed > 0) {
    process.exit(1);
  }
  console.log('SELFTEST OK');
}

main().catch((error) => {
  console.error(`FATAL: ${error && error.stack ? error.stack : error}`);
  process.exit(1);
});
