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
const attachmentLimitsJson = {
  maxUploadBytes: 7340032,
  maxRequestBytes: 10485760,
  maxAttachmentBytes: 33554432,
  image: {
    mimes: [
      { mime: 'image/png', maxBytes: 5242880 },
      { mime: 'image/jpeg', maxBytes: 5242880 },
      { mime: 'image/gif', maxBytes: 5242880 },
      { mime: 'image/webp', maxBytes: 5242880 },
    ],
    maxRequestBytes: 16777216,
  },
  document: {
    capable: true,
    mimes: [
      { mime: 'application/pdf', maxBytes: 8388608 },
      { mime: 'text/plain', maxBytes: 8388608 },
    ],
    maxRequestBytes: 16777216,
  },
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
  documentCapable: true,
  attachmentLimits: attachmentLimitsJson,
};

/** The shared daemon/IDE limit fixture (checked by the Rust side too). */
function attachmentLimitsFixture() {
  const url = new URL('../../../fixtures/attachment-limits.json', import.meta.url);
  return JSON.parse(readFileSync(url, 'utf8'));
}
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
// Audits 5/6/16: a PARTIAL generation with a capped fingerprint round.
const indexCoverageJson = {
  sessionId: '7',
  index_coverage: {
    workspace: 3,
    state: 'ready',
    generation: 2,
    published_generation: 2,
    coverage: {
      files_seen: 4500,
      files_indexed: 512,
      bytes_indexed: 2048,
      complete: false,
      truncated_reason: 'batch_files',
    },
    fingerprint: {
      scanned: 12,
      complete: false,
      shard: 3,
      round_start: 1,
      epoch: 4,
      shards_done: 7,
      verify: true,
      cursors: [],
      truncated_reason: 'fingerprint_files',
    },
    freshness: 'partial',
    serving: true,
  },
};
const indexCoverageUnhostedJson = { sessionId: '7', index_coverage: null };

/** One durable attachment REFERENCE fixture (the upload / ref metadata
 *  response shape): the surrogate ref id plus the exact metadata. */
const attachmentDigest = 'a'.repeat(64);
const attachmentRefJson = {
  ref_id: 3,
  digest: attachmentDigest,
  mime: 'application/pdf',
  filename: 'spec.pdf',
  size: 8,
};
const attachmentBytes = Buffer.from('%PDF-1.4');

// --------------------------------------------------------------- fake fetch

function jsonResponse(value, status = 200) {
  return new Response(JSON.stringify(value), {
    status,
    headers: { 'content-type': 'application/json' },
  });
}

function bytesResponse(value, contentType) {
  return new Response(value, {
    status: 200,
    headers: { 'content-type': contentType },
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
    const parsedModel = nc.validateModelCatalog([clone(modelInfoJson)])[0];
    assertEqual(parsedModel.documentCapable, true, 'advertised document gate parsed');
    assertEqual(parsedModel.attachmentLimits?.document.capable, true);
    assertEqual(parsedModel.attachmentLimits?.image.mimes[0].maxBytes, 5242880);
    assertEqual(parsedModel.attachmentLimits?.maxUploadBytes, 7340032);
    // Additive contract: a legacy daemon entry without the new fields keeps
    // validating (emergency fallback at the policy layer, never a throw).
    const legacyModel = clone(modelInfoJson);
    delete legacyModel.documentCapable;
    delete legacyModel.attachmentLimits;
    const legacyParsed = nc.validateModelCatalog([legacyModel])[0];
    assertEqual(legacyParsed.documentCapable, false);
    assertEqual(legacyParsed.attachmentLimits, null);
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
    assertEqual(
      nc.validateAttachmentRef(
        {
          ref_id: 3,
          digest: 'a'.repeat(64),
          mime: 'application/pdf',
          filename: 'spec.pdf',
          size: 8,
        },
        'test.ref',
      ).ref_id,
      3,
      'the generated ref validator keeps the surrogate reference identity',
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
      () => nc.validateModelCatalog([{ ...clone(modelInfoJson), documentCapable: 'yes' }]),
      'expected a boolean',
    );
    assertProtocol(
      () =>
        nc.validateModelCatalog([
          { ...clone(modelInfoJson), attachmentLimits: { ...clone(attachmentLimitsJson), maxUploadBytes: '7' } },
        ]),
      'attachmentLimits.maxUploadBytes',
    );
    assertProtocol(
      () =>
        nc.validateModelCatalog([
          {
            ...clone(modelInfoJson),
            attachmentLimits: {
              ...clone(attachmentLimitsJson),
              document: { ...clone(attachmentLimitsJson.document), capable: 'yes' },
            },
          },
        ]),
      'expected a boolean',
    );
    assertProtocol(
      () =>
        nc.validateModelCatalog([
          {
            ...clone(modelInfoJson),
            attachmentLimits: {
              ...clone(attachmentLimitsJson),
              image: {
                ...clone(attachmentLimitsJson.image),
                mimes: [{ mime: 'image/png' }],
              },
            },
          },
        ]),
      'missing required field maxBytes',
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
        nc.validatePermissionList({
          permissions: [{ id: '7', session_id: 9, capability: 'shell', detail: {} }],
        }),
      'expected a string, got number',
    );
    assertProtocol(
      () =>
        nc.validatePermissionList({
          permissions: [{ id: '7', capability: 'shell', detail: {} }],
        }),
      'missing required field session_id',
    );
    assertProtocol(() => nc.validatePermissionAck({ ok: 'yes' }), 'expected a boolean');
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
    assertProtocol(
      () =>
        nc.validateAttachmentRef(
          { ref_id: 0, digest: 'a'.repeat(64), mime: 'application/pdf', filename: null, size: 8 },
          'test.ref',
        ),
      'positive integer ref_id',
    );
    assertProtocol(
      () =>
        nc.validateAttachmentRef(
          { ref_id: 1, digest: 'a'.repeat(64), mime: 'application/pdf', filename: null },
          'test.ref',
        ),
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
    // Index coverage (audits 5/6/16): missing known fields, typed counters,
    // unknown freshness tags and a non-object response all fail loudly; a
    // null coverage is the ONLY honest "unhosted" answer.
    const coverageFixture = indexCoverageJson.index_coverage;
    assertProtocol(
      () => nc.validateIndexCoverage({ sessionId: '7', index_coverage: { ...clone(coverageFixture), freshness: 'sorta' } }),
      'unknown freshness',
    );
    const missingFreshness = clone(coverageFixture);
    delete missingFreshness.freshness;
    assertProtocol(
      () => nc.validateIndexCoverage({ sessionId: '7', index_coverage: missingFreshness }),
      'missing required field freshness',
    );
    assertProtocol(
      () =>
        nc.validateIndexCoverage({
          sessionId: '7',
          index_coverage: {
            ...clone(coverageFixture),
            coverage: { ...clone(coverageFixture.coverage), files_indexed: '512' },
          },
        }),
      'expected a finite number',
    );
    assertProtocol(
      () =>
        nc.validateIndexCoverage({
          sessionId: '7',
          index_coverage: {
            ...clone(coverageFixture),
            fingerprint: { ...clone(coverageFixture.fingerprint), verify: 'yes' },
          },
        }),
      'expected a boolean',
    );
    assertProtocol(() => nc.validateIndexCoverage('nope'), 'expected an object');
    assertEqual(
      nc.validateIndexCoverage(indexCoverageUnhostedJson),
      null,
      'null coverage is the honest unhosted answer',
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
      'POST /native/session/7/attachments': () => jsonResponse(attachmentRefJson),
      'GET /native/session/7/attachments/ref/3': () => jsonResponse(attachmentRefJson),
      [`GET /native/session/7/attachments/blob/${attachmentDigest}`]: () =>
        jsonResponse(attachmentRefJson),
      'GET /native/session/7/attachments/ref/3/bytes': () =>
        bytesResponse(attachmentBytes, 'application/pdf'),
      [`GET /native/session/7/attachments/blob/${attachmentDigest}/bytes`]: () =>
        bytesResponse(attachmentBytes, 'application/octet-stream'),
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
      'GET /native/index/coverage': (call) =>
        jsonResponse(call.query.session === '7' ? indexCoverageJson : indexCoverageUnhostedJson),
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
    const uploadedRef = await client.uploadAttachment('7', {
      mime: 'application/pdf',
      filename: 'spec.pdf',
      data_base64: 'eA==',
    });
    assertEqual(uploadedRef.ref_id, 3, 'the upload response keeps the surrogate ref id');
    assertDeepEqual(nc.attachmentIdOf(uploadedRef), {
      digest: attachmentDigest,
      mime: 'application/pdf',
      filename: 'spec.pdf',
      size: 8,
    });
    assertEqual((await client.attachmentReference('7', 3)).ref_id, 3);
    assertEqual(
      (await client.attachmentBlobReference('7', attachmentDigest)).ref_id,
      3,
      'the blob metadata route resolves the single reference',
    );
    const referenceBytes = await client.attachmentReferenceBytes('7', 3);
    assertEqual(referenceBytes.mime, 'application/pdf', 'ref bytes carry THAT reference MIME');
    assertDeepEqual([...referenceBytes.bytes], [...attachmentBytes]);
    assertDeepEqual(
      [...(await client.attachmentBlobBytes('7', attachmentDigest))],
      [...attachmentBytes],
      'blob bytes are the raw CAS bytes (no reference MIME invented)',
    );
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
    const coverage = await client.indexCoverage('7');
    assertEqual(coverage.coverage.files_indexed, 512);
    assertEqual(coverage.fingerprint.shard, 3);
    assertEqual(coverage.freshness, 'partial');
    assertEqual(coverage.serving, true);
    assertEqual(await client.indexCoverage('8'), null, 'an unhosted service is honestly null');
    assertEqual(nc.indexCoverageLabel(coverage), 'index: partial 512/4500 files');
    assertEqual(
      nc.indexCoverageLabel({
        ...coverage,
        freshness: 'stale_while_rebuilding',
        coverage: { ...coverage.coverage, complete: true },
      }),
      'index: stale while rebuilding',
    );
    assertEqual(nc.indexCoverageLabel(null), 'index: not reported');
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
    findCall(calls, 'GET', '/native/session/7/attachments/ref/3');
    findCall(calls, 'GET', `/native/session/7/attachments/blob/${attachmentDigest}`);
    findCall(calls, 'GET', '/native/session/7/attachments/ref/3/bytes');
    findCall(calls, 'GET', `/native/session/7/attachments/blob/${attachmentDigest}/bytes`);
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

  await test('index coverage never bleeds across a session switch or daemon stop', () => {
    const store = new st.FaktorStore();
    const coverage = indexCoverageJson.index_coverage;
    store.patch({
      daemon: 'running',
      session: { id: '7', title: 'a', provider: 'fake', model: 'm', state: 'open' },
    });
    store.patch({ indexCoverage: coverage });
    assertEqual(store.snapshot().indexCoverage.freshness, 'partial');
    // Same session, new read: the panel updates in place.
    store.patch({ indexCoverage: { ...coverage, freshness: 'current' } });
    assertEqual(store.snapshot().indexCoverage.freshness, 'current');
    // A different session must not inherit the previous workspace's coverage.
    store.patch({
      session: { id: '8', title: 'b', provider: 'fake', model: 'm', state: 'open' },
    });
    assertEqual(store.snapshot().indexCoverage, null);
    // A daemon stop drops it too (the daemon, not the session, owned it).
    store.patch({ daemon: 'stopped' });
    assertEqual(store.snapshot().indexCoverage, null);
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
    // The explicit result releases the single-flight lock before the next
    // logical submission (the lock itself is covered separately).
    deliver({ type: 'startResult', goal: 'plain goal', ok: true });
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
        return { ref_id: 2, digest: 'b'.repeat(64), mime: request.mime, filename: request.filename ?? null, size: 3 };
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
          return { ref_id: 3, digest: 'c'.repeat(64), mime: 'application/pdf', filename: null, size: 3 };
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

  await test('image submissions upload their bytes and start with the durable ids', async () => {
    const calls = [];
    const restores = [];
    const image = binaryAttachment({
      mime: 'image/png',
      filename: 'shot.png',
      isImage: true,
      bytes: 4,
      dataBase64: Buffer.from([137, 80, 78, 71]).toString('base64'),
    });
    const client = {
      uploadAttachment: async (sessionId, request) => {
        calls.push({ kind: 'upload', sessionId, request });
        return {
          ref_id: 4,
          digest: 'e'.repeat(64),
          mime: request.mime,
          filename: request.filename ?? null,
          size: 4,
        };
      },
      startTaskRun: async (sessionId, request) => {
        calls.push({ kind: 'start', sessionId, request });
        return taskRunStartedJson;
      },
    };
    const outcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({ attachments: [image] }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: (failure) => restores.push(failure),
    });
    assertEqual(outcome.ok, true, 'an allowlisted image is admitted client-side');
    assertEqual(restores.length, 0, 'success never restores');
    assertEqual(calls.length, 2, 'one upload then exactly one start');
    assertEqual(calls[0].kind, 'upload');
    assertDeepEqual(calls[0].request, {
      mime: 'image/png',
      filename: 'shot.png',
      data_base64: Buffer.from([137, 80, 78, 71]).toString('base64'),
    });
    assertEqual(calls[1].kind, 'start');
    assertDeepEqual(calls[1].request.attachments, [
      { digest: 'e'.repeat(64), mime: 'image/png', filename: 'shot.png', size: 4 },
    ]);
    assertEqual(outcome.attachmentIds.length, 1);
  });

  await test('undeliverable images are refused before any upload (mime allowlist + per-image bound)', async () => {
    const cases = [
      [
        'mime',
        binaryAttachment({ mime: 'image/svg+xml', filename: 'x.svg', isImage: true }),
        'unsupported_image_type',
      ],
      [
        'size',
        binaryAttachment({
          mime: 'image/png',
          filename: 'huge.png',
          isImage: true,
          bytes: ts.MAX_PENDING_IMAGE_BYTES + 1,
        }),
        'oversized_image',
      ],
    ];
    for (const [label, image, code] of cases) {
      const calls = [];
      const restores = [];
      const outcome = await ts.admitPendingSubmission({
        client: {
          uploadAttachment: async () => {
            calls.push('upload');
            throw new Error(`${label}: must not upload`);
          },
          startTaskRun: async () => {
            calls.push('start');
            throw new Error(`${label}: must not start`);
          },
        },
        sessionId: '7',
        pending: pendingEnvelope({ attachments: [image] }),
        settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
        onStarted: () => {},
        onFailure: () => {},
        restore: (failure) => restores.push(failure),
      });
      assertEqual(outcome.ok, false, label);
      assertDeepEqual(calls, [], `${label}: refused before any upload/start request`);
      assertEqual(restores.length, 1, `${label}: restore exactly once`);
      assertEqual(restores[0].kind, 'image_unsupported', label);
      assertEqual(restores[0].code, code, label);
      assertEqual(outcome.attachmentIds.length, 0, label);
    }
  });

  await test('the advertised model contract drives attachment gates; the emergency ceiling is only the fallback', () => {
    const fixture = attachmentLimitsFixture();
    // The shared fixture is the Rust-checked canonical contract.
    const advertised = ts.attachmentPolicyFromLimits(fixture.canonical);
    assertEqual(advertised.source, 'advertised');
    assertEqual(advertised.maxUploadBytes, fixture.canonical.maxUploadBytes);
    assertEqual(advertised.maxImageBytes, 5242880);
    assertEqual(advertised.documentCapable, true);
    assertDeepEqual(advertised.documentMimes, ['application/pdf', 'text/plain']);
    // The emergency fallback is byte-identical to the daemon constants and
    // CONSERVATIVE: never above any advertised bound.
    assertEqual(ts.EMERGENCY_ATTACHMENT_POLICY.source, 'emergency');
    assertEqual(ts.EMERGENCY_ATTACHMENT_POLICY.maxUploadBytes, ts.MAX_PENDING_ATTACHMENT_BYTES);
    assertEqual(ts.EMERGENCY_ATTACHMENT_POLICY.maxUploadBytes, fixture.emergencyCeiling.maxUploadBytes);
    assertEqual(ts.EMERGENCY_ATTACHMENT_POLICY.maxImageBytes, fixture.emergencyCeiling.maxImageBytes);
    assertEqual(ts.EMERGENCY_ATTACHMENT_POLICY.maxDocumentBytes, fixture.emergencyCeiling.maxDocumentBytes);
    assertDeepEqual([...ts.SUPPORTED_PENDING_IMAGE_MIMES], fixture.emergencyCeiling.imageMimes);
    assertDeepEqual([...ts.SUPPORTED_PENDING_DOCUMENT_MIMES], fixture.emergencyCeiling.documentMimes);
    for (const entry of fixture.canonical.image.mimes) {
      assert(
        ts.EMERGENCY_ATTACHMENT_POLICY.maxImageBytes <= entry.maxBytes,
        `emergency image ceiling must stay <= advertised ${entry.mime}`,
      );
    }
    for (const entry of fixture.canonical.document.mimes) {
      assert(
        ts.EMERGENCY_ATTACHMENT_POLICY.maxDocumentBytes <= entry.maxBytes,
        `emergency document ceiling must stay <= advertised ${entry.mime}`,
      );
    }
    // Catalog resolution: the exact (provider, model) entry wins; a missing
    // model falls back to the emergency ceiling, never to no ceiling.
    const catalog = [nc.validateModelCatalog([{ ...clone(modelInfoJson), provider: 'p', model: 'm' }])[0]];
    assertEqual(ts.attachmentPolicyForModel(catalog, 'p', 'm').source, 'advertised');
    assertEqual(ts.attachmentPolicyForModel(catalog, 'p', 'unknown'), ts.EMERGENCY_ATTACHMENT_POLICY);
  });

  await test('advertised image bounds gate before upload (tighter AND looser than the emergency ceiling)', () => {
    const fixture = attachmentLimitsFixture();
    const image = (bytes) =>
      binaryAttachment({ mime: 'image/png', filename: 'image.png', isImage: true, bytes });
    // A tighter advertised per-image bound wins over the emergency 5 MiB.
    const tight = ts.attachmentPolicyFromLimits({
      ...clone(fixture.canonical),
      image: {
        ...clone(fixture.canonical.image),
        mimes: fixture.canonical.image.mimes.map((entry) => ({ ...entry, maxBytes: 1024 })),
      },
    });
    const refusal = ts.pendingImageRefusal([image(2048)], tight);
    assertEqual(refusal.kind, 'image_unsupported');
    assertEqual(refusal.code, 'oversized_image');
    assert(refusal.message.includes('1024'), refusal.message);
    // A looser advertised bound (20 MiB per image) admits an 8 MiB image
    // that the emergency fallback would refuse — consumed, not mirrored.
    const loose = ts.attachmentPolicyFromLimits({
      ...clone(fixture.canonical),
      image: {
        ...clone(fixture.canonical.image),
        mimes: fixture.canonical.image.mimes.map((entry) => ({ ...entry, maxBytes: 20 * 1024 * 1024 })),
      },
    });
    assertEqual(ts.pendingImageRefusal([image(8 * 1024 * 1024)], loose), null);
    assertEqual(
      ts.pendingImageRefusal([image(8 * 1024 * 1024)], ts.EMERGENCY_ATTACHMENT_POLICY).code,
      'oversized_image',
      'without the catalog the conservative ceiling still refuses',
    );
    // The advertised request-wide image total is enforced too: four 4.5 MiB
    // images each fit the 5 MiB per-image bound but total 18 MiB > 16 MiB.
    const total = ts.pendingImageRefusal(
      [image(4_500_000), image(4_500_000), image(4_500_000), image(4_500_000)],
      ts.attachmentPolicyFromLimits(fixture.canonical),
    );
    assertEqual(total.code, 'oversized_image');
    assert(total.message.includes('request image bound'), total.message);
  });

  await test('documents ride the advertised capability contract (legacy daemons keep deciding)', async () => {
    const fixture = attachmentLimitsFixture();
    const pdf = binaryAttachment({ bytes: 3 });
    // Advertised capable: the document passes the client gate.
    assertEqual(ts.pendingDocumentRefusal([pdf], ts.attachmentPolicyFromLimits(fixture.canonical)), null);
    // Advertised incapable: refused before any upload, with the model reason.
    const incapable = ts.attachmentPolicyFromLimits({
      ...clone(fixture.canonical),
      document: { ...clone(fixture.canonical.document), capable: false },
    });
    const refusal = ts.pendingDocumentRefusal([pdf], incapable);
    assertEqual(refusal.kind, 'document_unsupported');
    assertEqual(refusal.code, 'unsupported_document_type');
    assert(refusal.message.includes('document'), refusal.message);
    // Legacy daemon (no advertised contract): the daemon's own admission
    // decides, so the client does not refuse on capability grounds.
    assertEqual(ts.pendingDocumentRefusal([pdf], ts.EMERGENCY_ATTACHMENT_POLICY), null);
    // Advertised per-document bound is consumed.
    const smallDocs = ts.attachmentPolicyFromLimits({
      ...clone(fixture.canonical),
      document: {
        ...clone(fixture.canonical.document),
        mimes: fixture.canonical.document.mimes.map((entry) => ({ ...entry, maxBytes: 1024 })),
      },
    });
    assertEqual(
      ts.pendingDocumentRefusal([binaryAttachment({ bytes: 2048 })], smallDocs).code,
      'oversized_document',
    );
    // The advertised upload ceiling refuses end-to-end BEFORE any request.
    const calls = [];
    const restores = [];
    const outcome = await ts.admitPendingSubmission({
      client: {
        uploadAttachment: async () => {
          calls.push('upload');
          throw new Error('must not upload');
        },
        startTaskRun: async () => {
          calls.push('start');
          throw new Error('must not start');
        },
      },
      sessionId: '7',
      pending: pendingEnvelope({ attachments: [pdf] }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      attachmentLimits: ts.attachmentPolicyFromLimits({
        ...clone(fixture.canonical),
        maxUploadBytes: 2,
      }),
      onStarted: () => {},
      onFailure: () => {},
      restore: (failure) => restores.push(failure),
    });
    assertEqual(outcome.ok, false);
    assertDeepEqual(calls, [], 'refused before any upload/start request');
    assertEqual(outcome.failure.code, 'oversized_upload');
    assertEqual(restores.length, 1);
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
          return { ref_id: 5, digest: 'd'.repeat(64), mime: request.mime, filename: request.filename ?? null, size: 8 };
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

  await test('a failed start keeps the uploaded id in local pending state and the retry uploads nothing', async () => {
    const uploads = [];
    const starts = [];
    let failStart = true;
    const client = {
      uploadAttachment: async (sessionId, request) => {
        uploads.push(request.filename);
        return { ref_id: 6, digest: 'f'.repeat(64), mime: request.mime, filename: request.filename ?? null, size: 3 };
      },
      startTaskRun: async (sessionId, request) => {
        starts.push(request.attachments);
        if (failStart) {
          throw new nc.NativeApiError(409, 'conflict', 'session already has a live run', false);
        }
        return taskRunStartedJson;
      },
    };
    const restores = [];
    const first = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({ attachments: [binaryAttachment()] }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: (failure, enriched) => restores.push({ failure, enriched }),
    });
    assertEqual(first.ok, false);
    assertEqual(uploads.length, 1, 'exactly one upload on the first attempt');
    assertEqual(restores.length, 1, 'the failed start restores with the enriched envelope');
    const retained = restores[0].enriched;
    assert(retained.attachments[0].uploaded !== undefined, 'the durable id is retained in pending state');
    assertEqual(retained.attachments[0].uploaded.sessionId, '7');
    assertEqual(retained.attachments[0].uploaded.attachment.digest, 'f'.repeat(64));
    assertEqual(first.pending.attachments[0].uploaded.attachment.digest, 'f'.repeat(64));

    failStart = false;
    const retry = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: retained,
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {
        throw new Error('a successful retry must not restore');
      },
    });
    assertEqual(retry.ok, true);
    assertEqual(uploads.length, 1, 'the retry resolves the durable id first and uploads no bytes');
    assertDeepEqual(starts[1], [
      { digest: 'f'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 3 },
    ]);
  });

  await test('an upload retained for one session is never reused by another session', async () => {
    const uploads = [];
    const starts = [];
    let firstRun = true;
    const client = {
      uploadAttachment: async (sessionId, request) => {
        uploads.push({ sessionId, filename: request.filename });
        return {
          ref_id: sessionId === '7' ? 7 : 8,
          digest: (sessionId === '7' ? 'a' : 'b').repeat(64),
          mime: request.mime,
          filename: request.filename ?? null,
          size: 3,
        };
      },
      startTaskRun: async (sessionId, request) => {
        starts.push({ sessionId, attachments: request.attachments });
        if (firstRun) {
          firstRun = false;
          throw new Error('socket closed');
        }
        return taskRunStartedJson;
      },
    };
    const restores = [];
    await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({ attachments: [binaryAttachment()] }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: (failure, enriched) => restores.push(enriched),
    });
    const retained = restores[0];
    assertEqual(retained.attachments[0].uploaded.sessionId, '7');
    const retry = await ts.admitPendingSubmission({
      client,
      sessionId: '8',
      pending: retained,
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {},
    });
    assertEqual(retry.ok, true);
    assertEqual(uploads.length, 2, 'the foreign-session id must not be reused');
    assertEqual(uploads[1].sessionId, '8');
    assertDeepEqual(starts[1].attachments, [
      { digest: 'b'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 3 },
    ]);
  });

  await test('a partial upload failure retries only the absent attachments', async () => {
    let calls = 0;
    const uploadedNames = [];
    const client = {
      uploadAttachment: async (sessionId, request) => {
        calls += 1;
        if (calls === 2) {
          throw new nc.NativeApiError(413, 'oversized', 'attachment exceeds the bound', false);
        }
        uploadedNames.push(request.filename);
        return {
          ref_id: calls,
          digest: String(calls).padStart(64, '0'),
          mime: request.mime,
          filename: request.filename ?? null,
          size: 3,
        };
      },
      startTaskRun: async () => taskRunStartedJson,
    };
    const restores = [];
    const first = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({
        attachments: [binaryAttachment(), binaryAttachment({ filename: 'second.pdf' })],
      }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: (failure, enriched) => restores.push(enriched),
    });
    assertEqual(first.ok, false);
    assertEqual(first.failure.code, 'oversized');
    assertEqual(uploadedNames.length, 1, 'the first attachment uploaded before the refusal');
    const retained = restores[0];
    assert(retained.attachments[0].uploaded !== undefined, 'the first upload is retained');
    assertEqual(retained.attachments[1].uploaded, undefined, 'the failed attachment is not retained');
    const retry = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: retained,
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {
        throw new Error('the retry must succeed');
      },
    });
    assertEqual(retry.ok, true);
    assertEqual(calls, 3, 'the retry uploads only the second attachment');
    assertEqual(uploadedNames.length, 2);
    assertEqual(retry.attachmentIds.length, 2);
    assertEqual(retry.pending.attachments[0].uploaded.attachment.digest, '1'.padStart(64, '0'));
    assertEqual(retry.pending.attachments[1].uploaded.attachment.digest, '3'.padStart(64, '0'));
  });

  await test('hostile already-uploaded records are refused at the envelope boundary', () => {
    const validUpload = {
      sessionId: '7',
      contentDigest: ts.pendingAttachmentContentDigest(binaryAttachment()),
      attachment: { ref_id: 23, digest: 'a'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 3 },
    };
    const parsed = ts.parsePendingSubmission({
      ...pendingEnvelope(),
      attachments: [{ ...binaryAttachment(), uploaded: validUpload }],
    });
    assert(parsed !== null, 'a strict uploaded record is accepted');
    assertDeepEqual(parsed.attachments[0].uploaded, validUpload);
    for (const uploaded of [
      'nope',
      { sessionId: '7' },
      { ...validUpload, contentDigest: undefined },
      { ...validUpload, contentDigest: 'not-hex' },
      { ...validUpload, contentDigest: 'A'.repeat(64) },
      { ...validUpload, contentDigest: 'a'.repeat(63) },
      { sessionId: '7', attachment: { digest: 'not-hex', mime: 'application/pdf', filename: null, size: 3 } },
      { sessionId: '', attachment: validUpload.attachment },
      { sessionId: '7', attachment: { digest: 'a'.repeat(64), mime: 'application/pdf', filename: null, size: -1 } },
      { sessionId: '7', attachment: { digest: 'a'.repeat(64), mime: 'application/pdf', filename: 9, size: 3 } },
      { sessionId: '7', attachment: { digest: 'a'.repeat(64), mime: 'application/pdf', filename: null, size: 3 } },
      {
        sessionId: '7',
        attachment: { ref_id: 0, digest: 'a'.repeat(64), mime: 'application/pdf', filename: null, size: 3 },
      },
      {
        sessionId: '7',
        attachment: { ref_id: 1.5, digest: 'a'.repeat(64), mime: 'application/pdf', filename: null, size: 3 },
      },
      {
        sessionId: '7',
        attachment: { ref_id: '1', digest: 'a'.repeat(64), mime: 'application/pdf', filename: null, size: 3 },
      },
    ]) {
      assertEqual(
        ts.parsePendingSubmission({
          ...pendingEnvelope(),
          attachments: [{ ...binaryAttachment(), uploaded }],
        }),
        null,
        JSON.stringify(uploaded).slice(0, 160),
      );
    }
  });

  await test('an upload keeps its ref_id through admission and the ref routes retrieve per-reference metadata', async () => {
    const digest = 'b'.repeat(64);
    const refA = { ref_id: 11, digest, mime: 'application/pdf', filename: 'spec.pdf', size: 8 };
    const refB = { ref_id: 12, digest, mime: 'text/plain', filename: 'notes.txt', size: 8 };
    const bytes = Buffer.from('%PDF-1.4');
    const { client, calls } = makeClient({
      'POST /native/session/7/attachments': () => jsonResponse(refA),
      'GET /native/session/7/attachments/ref/11': () => jsonResponse(refA),
      'GET /native/session/7/attachments/ref/12': () => jsonResponse(refB),
      'GET /native/session/7/attachments/ref/11/bytes': () => bytesResponse(bytes, 'application/pdf'),
      'GET /native/session/7/attachments/ref/12/bytes': () => bytesResponse(bytes, 'text/plain'),
      [`GET /native/session/7/attachments/blob/${digest}/bytes`]: () =>
        bytesResponse(bytes, 'application/octet-stream'),
      'POST /native/session/7/task-runs': () => jsonResponse(taskRunStartedJson),
    });
    const uploaded = await client.uploadAttachment('7', {
      mime: 'application/pdf',
      filename: 'spec.pdf',
      data_base64: 'JVBERi0xLjQ=',
    });
    assertEqual(uploaded.ref_id, 11, 'the upload retains the reference identity');
    const outcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({
        attachments: [binaryAttachment({ bytes: 8, dataBase64: 'JVBERi0xLjQ=' })],
      }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {
        throw new Error('the admission must succeed');
      },
    });
    assertEqual(outcome.ok, true);
    // The retained identity keeps ref_id; task admission projects the SAME
    // digest/mime/filename/size (the wire projection carries no ref_id).
    assertEqual(outcome.pending.attachments[0].uploaded.attachment.ref_id, 11);
    assertDeepEqual(outcome.pending.attachments[0].uploaded.attachment, refA);
    const start = calls.find(
      (call) => call.method === 'POST' && call.path === '/native/session/7/task-runs',
    );
    assertDeepEqual(start.body.attachments, [
      { digest, mime: 'application/pdf', filename: 'spec.pdf', size: 8 },
    ]);
    // Two references share the digest: every ref-addressed retrieval keeps
    // its OWN metadata/MIME, while blob bytes are the raw CAS bytes.
    assertDeepEqual(await client.attachmentReference('7', 11), refA);
    assertDeepEqual(await client.attachmentReference('7', 12), refB);
    assertEqual((await client.attachmentReferenceBytes('7', 11)).mime, 'application/pdf');
    assertEqual((await client.attachmentReferenceBytes('7', 12)).mime, 'text/plain');
    assertDeepEqual([...(await client.attachmentBlobBytes('7', digest))], [...bytes]);
  });

  await test('the retainer bounds retry state and merges uploads only by content identity', () => {
    const retainer = new ts.PendingSubmissionRetainer(2);
    const envelope = (messageId, bytes = '%PDF') =>
      ts.parsePendingSubmission({
        text: 'ship it',
        sessionId: '7',
        messageId,
        draftId: null,
        files: [],
        attachments: [binaryAttachment({ dataBase64: Buffer.from(bytes).toString('base64') })],
      });
    const withUpload = (pending, digest) =>
      ts.withPendingUpload(pending, 0, {
        sessionId: '7',
        contentDigest: ts.pendingAttachmentContentDigest(pending.attachments[0]),
        attachment: { ref_id: 24, digest, mime: 'application/pdf', filename: 'spec.pdf', size: 3 },
      });
    const first = withUpload(envelope('m1'), 'a'.repeat(64));
    retainer.retain(first);
    assertEqual(retainer.size(), 1);
    const restored = retainer.restore(envelope('m1'));
    assertEqual(restored.attachments[0].uploaded.attachment.digest, 'a'.repeat(64));
    assertEqual(retainer.restore(envelope('m2')).attachments[0].uploaded, undefined);
    retainer.release(first);
    assertEqual(retainer.size(), 0, 'a durable acceptance releases the entry');
    retainer.retain(withUpload(envelope('m1'), 'a'.repeat(64)));
    retainer.retain(withUpload(envelope('m2'), 'b'.repeat(64)));
    retainer.retain(withUpload(envelope('m3'), 'c'.repeat(64)));
    assertEqual(retainer.size(), 2, 'the retry state is bounded');
    assertEqual(
      retainer.restore(envelope('m1')).attachments[0].uploaded,
      undefined,
      'the oldest identity was evicted',
    );
  });

  await test('retry reuse matches content identity, never array index', async () => {
    // Same draft identity, same attachment count, same slot: the bytes
    // changed, so the retained upload must NOT be reused and a fresh upload
    // runs; unchanged bytes DO reuse the durable id.
    const retainer = new ts.PendingSubmissionRetainer();
    const envelope = (bytes) =>
      ts.parsePendingSubmission({
        text: 'ship it',
        sessionId: '7',
        messageId: 'same-draft',
        draftId: null,
        files: [],
        attachments: [binaryAttachment({ dataBase64: Buffer.from(bytes).toString('base64') })],
      });
    const before = envelope('BBBB');
    const unchanged = envelope('BBBB');
    const changed = envelope('CCCC');
    assertEqual(unchanged.attachments.length, changed.attachments.length, 'same attachment count');
    retainer.retain(
      ts.withPendingUpload(before, 0, {
        sessionId: '7',
        contentDigest: ts.pendingAttachmentContentDigest(before.attachments[0]),
        attachment: { ref_id: 21, digest: 'a'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 4 },
      }),
    );
    assertEqual(
      retainer.restore(changed).attachments[0].uploaded,
      undefined,
      'changed bytes under the same draft identity must not reuse the old id',
    );
    assertEqual(
      retainer.restore(unchanged).attachments[0].uploaded.attachment.digest,
      'a'.repeat(64),
      'unchanged bytes reuse the retained id',
    );

    // The same rule holds at admission time when an envelope carries a
    // stale `uploaded` record whose content digest no longer matches.
    const uploads = [];
    const client = {
      uploadAttachment: async (sessionId, request) => {
        uploads.push(request.data_base64);
        return {
          ref_id: 9,
          digest: 'f'.repeat(64),
          mime: request.mime,
          filename: request.filename ?? null,
          size: request.data_base64.length,
        };
      },
      startTaskRun: async () => taskRunStartedJson,
    };
    const changedParsed = ts.parsePendingSubmission({
      text: 'ship it',
      sessionId: '7',
      messageId: 'same-draft',
      draftId: null,
      files: [],
      attachments: [{ ...binaryAttachment({ dataBase64: Buffer.from('CCCC').toString('base64') }) }],
    });
    const stalePending = {
      ...changedParsed,
      attachments: [
        {
          ...changedParsed.attachments[0],
          uploaded: {
            sessionId: '7',
            contentDigest: ts.pendingAttachmentContentDigest(unchanged.attachments[0]),
            attachment: { ref_id: 21, digest: 'a'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 4 },
          },
        },
      ],
    };
    const staleOutcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: stalePending,
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {},
    });
    assertEqual(staleOutcome.ok, true);
    assertEqual(uploads.length, 1, 'stale content digest forces a fresh upload');

    const matchingPending = {
      ...changedParsed,
      attachments: [
        {
          ...changedParsed.attachments[0],
          uploaded: {
            sessionId: '7',
            contentDigest: ts.pendingAttachmentContentDigest(changedParsed.attachments[0]),
            attachment: { ref_id: 22, digest: 'b'.repeat(64), mime: 'application/pdf', filename: 'spec.pdf', size: 4 },
          },
        },
      ],
    };
    const matchingOutcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: matchingPending,
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {
        throw new Error('a reuse admission must succeed');
      },
    });
    assertEqual(matchingOutcome.ok, true);
    assertEqual(uploads.length, 1, 'matching content digest reuses the id and uploads nothing');
    assertEqual(matchingOutcome.attachmentIds[0], 'b'.repeat(64));
  });

  await test('retry identity includes filename and size and ignores array position', async () => {
    // Same bytes and mime, different filename or declared size: a
    // rename/re-select is a different reference and must force a fresh
    // upload; an unchanged reference is position-independent.
    const named = binaryAttachment({ filename: 'spec.pdf' });
    const renamed = binaryAttachment({ filename: 'renamed.pdf' });
    const resized = binaryAttachment({ filename: 'spec.pdf', bytes: 4 });
    const moved = binaryAttachment({ filename: 'spec.pdf' });
    assert(
      ts.pendingAttachmentContentDigest(named) !== ts.pendingAttachmentContentDigest(renamed),
      'a changed filename must change the retry identity',
    );
    assert(
      ts.pendingAttachmentContentDigest(named) !== ts.pendingAttachmentContentDigest(resized),
      'a changed declared size must change the retry identity',
    );
    assertEqual(
      ts.pendingAttachmentContentDigest(named),
      ts.pendingAttachmentContentDigest(moved),
      'the same exact reference is position-independent',
    );

    const retainer = new ts.PendingSubmissionRetainer();
    const envelope = (messageId, attachments) =>
      ts.parsePendingSubmission({
        text: 'ship it',
        sessionId: '7',
        messageId,
        draftId: null,
        files: [],
        attachments,
      });
    const before = envelope('rename-draft', [named]);
    retainer.retain(
      ts.withPendingUpload(before, 0, {
        sessionId: '7',
        contentDigest: ts.pendingAttachmentContentDigest(before.attachments[0]),
        attachment: {
          ref_id: 25,
          digest: 'a'.repeat(64),
          mime: named.mime,
          filename: named.filename,
          size: 3,
        },
      }),
    );
    assertEqual(
      retainer.restore(envelope('rename-draft', [renamed])).attachments[0].uploaded,
      undefined,
      'a renamed file under the same draft identity must not reuse the old id',
    );
    assertEqual(
      retainer.restore(envelope('rename-draft', [moved])).attachments[0].uploaded.attachment.digest,
      'a'.repeat(64),
      'the unchanged reference reuses the retained id',
    );

    // Order-only changes keep every reference's id: two attachments that
    // swap positions still match their retained records by the full digest.
    const firstRef = binaryAttachment({
      filename: 'one.pdf',
      dataBase64: Buffer.from('ONE').toString('base64'),
    });
    const secondRef = binaryAttachment({
      mime: 'text/plain',
      filename: 'two.txt',
      dataBase64: Buffer.from('TWO').toString('base64'),
    });
    const pair = envelope('order-draft', [firstRef, secondRef]);
    let withUploads = ts.withPendingUpload(pair, 0, {
      sessionId: '7',
      contentDigest: ts.pendingAttachmentContentDigest(firstRef),
      attachment: {
        ref_id: 26,
        digest: '1'.repeat(64),
        mime: firstRef.mime,
        filename: firstRef.filename,
        size: 3,
      },
    });
    withUploads = ts.withPendingUpload(withUploads, 1, {
      sessionId: '7',
      contentDigest: ts.pendingAttachmentContentDigest(secondRef),
      attachment: {
        ref_id: 27,
        digest: '2'.repeat(64),
        mime: secondRef.mime,
        filename: secondRef.filename,
        size: 3,
      },
    });
    retainer.retain(withUploads);
    const restored = retainer.restore(envelope('order-draft', [secondRef, firstRef]));
    assertEqual(
      restored.attachments[0].uploaded.attachment.digest,
      '2'.repeat(64),
      'the reordered second reference keeps its own id',
    );
    assertEqual(
      restored.attachments[1].uploaded.attachment.digest,
      '1'.repeat(64),
      'the reordered first reference keeps its own id',
    );
  });

  await test('documents upload as durable attachments when the advertised model supports them', async () => {
    const calls = [];
    const client = {
      uploadAttachment: async (sessionId, request) => {
        calls.push({ kind: 'upload', mime: request.mime, filename: request.filename });
        return {
          ref_id: request.mime === 'application/pdf' ? 31 : 32,
          digest: (request.mime === 'application/pdf' ? 'a' : 'b').repeat(64),
          mime: request.mime,
          filename: request.filename ?? null,
          size: 3,
        };
      },
      startTaskRun: async (sessionId, request) => {
        calls.push({ kind: 'start', request });
        return taskRunStartedJson;
      },
    };
    const outcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({
        attachments: [
          binaryAttachment({ mime: 'application/pdf', filename: 'spec.pdf' }),
          binaryAttachment({
            mime: 'text/plain',
            filename: 'notes.txt',
            dataBase64: Buffer.from('txt').toString('base64'),
          }),
        ],
      }),
      settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
      attachmentLimits: ts.attachmentPolicyFromLimits(clone(attachmentLimitsJson)),
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {
        throw new Error('an accepted document must not restore');
      },
    });
    assertEqual(outcome.ok, true);
    assertDeepEqual(
      calls.filter((call) => call.kind === 'upload').map((call) => call.mime),
      ['application/pdf', 'text/plain'],
      'both advertised document types upload',
    );
    const start = calls.find((call) => call.kind === 'start');
    assertDeepEqual(
      start.request.attachments.map((id) => id.mime),
      ['application/pdf', 'text/plain'],
      'the run carries the durable document ids in entry order',
    );
    assertEqual(start.request.files, undefined, 'documents never ride the workspace path vocabulary');
  });

  await test('an advertised-unavailable document refuses before any upload', async () => {
    const calls = [];
    const client = {
      uploadAttachment: async () => {
        calls.push('upload');
        throw new Error('a refused attachment must not upload');
      },
      startTaskRun: async () => {
        calls.push('start');
        throw new Error('a refused attachment must not start');
      },
    };
    const incapable = clone(attachmentLimitsJson);
    incapable.document.capable = false;
    const pdfOnly = clone(attachmentLimitsJson);
    pdfOnly.document.mimes = [{ mime: 'application/pdf', maxBytes: 8388608 }];
    const tinyDocs = clone(attachmentLimitsJson);
    tinyDocs.document.mimes = tinyDocs.document.mimes.map((entry) => ({ ...entry, maxBytes: 2 }));
    const tinyUpload = clone(attachmentLimitsJson);
    tinyUpload.maxUploadBytes = 1;
    for (const [label, limits, attachment, code] of [
      ['capability', incapable, binaryAttachment(), 'unsupported_document_type'],
      ['mime', pdfOnly, binaryAttachment({ mime: 'text/plain' }), 'unsupported_document_type'],
      ['size', tinyDocs, binaryAttachment({ bytes: 3 }), 'oversized_document'],
      ['upload', tinyUpload, binaryAttachment({ bytes: 3 }), 'oversized_upload'],
    ]) {
      const failures = [];
      let restored = 0;
      let started = 0;
      const outcome = await ts.admitPendingSubmission({
        client,
        sessionId: '7',
        pending: pendingEnvelope({ attachments: [attachment] }),
        settings: { mutationMode: '', maxTokens: 0, maxCostMicro: 0n },
        attachmentLimits: ts.attachmentPolicyFromLimits(limits),
        onStarted: () => {
          started += 1;
        },
        onFailure: (failure) => failures.push(failure),
        restore: () => {
          restored += 1;
        },
      });
      assertEqual(outcome.ok, false, label);
      assertEqual(outcome.failure.code, code, label);
      assertEqual(failures.length, 1, `${label}: exactly one typed failure`);
      assertEqual(started, 0, `${label}: no run starts`);
      assertEqual(restored, 1, `${label}: the draft is restored exactly once`);
    }
    assertEqual(calls.length, 0, 'a refused attachment never reaches the wire');
  });

  await test('workspace source files stay on the repository-context path and are never uploaded', async () => {
    const uploads = [];
    const starts = [];
    const client = {
      uploadAttachment: async () => {
        uploads.push('upload');
        throw new Error('source files must not upload');
      },
      startTaskRun: async (sessionId, request) => {
        starts.push(request);
        return taskRunStartedJson;
      },
    };
    const outcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: pendingEnvelope({ files: ['src/a.ts', 'crates/b.rs'], attachments: [] }),
      settings: {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        files: ['src/a.ts', 'crates/b.rs'],
      },
      onStarted: () => {},
      onFailure: () => {
        throw new Error('a source-only start must not fail');
      },
      restore: () => {
        throw new Error('a source-only start must not restore');
      },
    });
    assertEqual(outcome.ok, true);
    assertEqual(uploads.length, 0, 'no upload for workspace source files');
    assertDeepEqual(starts[0].files, ['src/a.ts', 'crates/b.rs'], 'paths stay repository-context');
    assertEqual(starts[0].attachments, undefined, 'source files never become binary attachments');
  });
}

// -------- 7b-3. attachment HTTP retrieval contract (audit findings 2/4)

async function attachmentHttpContractTests() {
  await test('attachment byte routes use the 7 MiB HTTP contract, not the 4 MiB generic cap', async () => {
    // The generic cap stays 4 MiB; the attachment bound mirrors the daemon's
    // HTTP upload/retrieval contract. A 4-7 MiB admission must round-trip.
    assertEqual(nc.DEFAULT_MAX_BODY_BYTES, 4 * 1024 * 1024, 'the generic body cap stays 4 MiB');
    assertEqual(
      nc.ATTACHMENT_RESPONSE_MAX_BYTES,
      7 * 1024 * 1024,
      'the attachment response bound mirrors the daemon HTTP contract',
    );
    assert(
      nc.ATTACHMENT_RESPONSE_MAX_BYTES > nc.DEFAULT_MAX_BODY_BYTES,
      'the old 4 MiB generic cap would refuse a 4-7 MiB retrieval',
    );
    const mib = 1024 * 1024;
    // Generated bounded payloads (no embedded literals): 4 MiB + 1 and
    // exactly 7 MiB fit the retrieval contract; 7 MiB + 1 is refused typed.
    const justOverFour = new Uint8Array(4 * mib + 1);
    const seven = new Uint8Array(7 * mib);
    const overSeven = new Uint8Array(7 * mib + 1);
    for (const buffer of [justOverFour, seven, overSeven]) {
      buffer[0] = 0x5a;
      buffer[buffer.length - 1] = 0xa5;
    }
    const digestFour = 'a'.repeat(64);
    const digestSeven = 'b'.repeat(64);
    const digestOver = 'c'.repeat(64);
    const { client, calls } = makeClient({
      'GET /native/session/7/attachments/ref/1/bytes': () =>
        bytesResponse(justOverFour, 'application/pdf'),
      'GET /native/session/7/attachments/ref/2/bytes': () =>
        bytesResponse(seven, 'application/pdf'),
      'GET /native/session/7/attachments/ref/3/bytes': () =>
        bytesResponse(overSeven, 'application/pdf'),
      [`GET /native/session/7/attachments/blob/${digestFour}/bytes`]: () =>
        bytesResponse(justOverFour, 'application/octet-stream'),
      [`GET /native/session/7/attachments/blob/${digestSeven}/bytes`]: () =>
        bytesResponse(seven, 'application/octet-stream'),
      [`GET /native/session/7/attachments/blob/${digestOver}/bytes`]: () =>
        bytesResponse(overSeven, 'application/octet-stream'),
    });
    const refFour = await client.attachmentReferenceBytes('7', 1);
    assertEqual(
      refFour.bytes.byteLength,
      4 * mib + 1,
      'a 4 MiB + 1 ref payload exceeds the old cap and still retrieves',
    );
    assertEqual(refFour.bytes[0], 0x5a, 'the first byte survives byte-exact');
    assertEqual(refFour.bytes[refFour.bytes.byteLength - 1], 0xa5, 'the last byte survives byte-exact');
    const refSeven = await client.attachmentReferenceBytes('7', 2);
    assertEqual(refSeven.bytes.byteLength, 7 * mib, 'exactly 7 MiB ref bytes retrieve byte-exact');
    const blobFour = await client.attachmentBlobBytes('7', digestFour);
    assertEqual(
      blobFour.byteLength,
      4 * mib + 1,
      'a 4 MiB + 1 blob payload exceeds the old cap and still retrieves',
    );
    const blobSeven = await client.attachmentBlobBytes('7', digestSeven);
    assertEqual(blobSeven.byteLength, 7 * mib, 'exactly 7 MiB blob bytes retrieve byte-exact');
    findCall(calls, 'GET', '/native/session/7/attachments/ref/1/bytes');
    findCall(calls, 'GET', '/native/session/7/attachments/ref/2/bytes');
    findCall(calls, 'GET', `/native/session/7/attachments/blob/${digestFour}/bytes`);
    findCall(calls, 'GET', `/native/session/7/attachments/blob/${digestSeven}/bytes`);
    // One byte over the contract is refused typed on BOTH byte routes.
    for (const [label, call] of [
      ['ref', () => client.attachmentReferenceBytes('7', 3)],
      ['blob', () => client.attachmentBlobBytes('7', digestOver)],
    ]) {
      await assertRejects(
        call,
        (error) =>
          error instanceof nc.NativeProtocolError &&
          error.message.includes(`exceeded bound ${7 * mib}`),
        `${label} 7 MiB + 1`,
      );
    }
    // The generic body cap is NOT raised for every request: an ordinary JSON
    // response over the configured bound still fails, so the attachment
    // bound is a contract, not a global loosening.
    const tiny = makeClient(
      { 'GET /native/session/7/tasks': () => jsonResponse([clone(taskViewJson)]) },
      { maxBodyBytes: 8 },
    );
    await assertRejects(
      () => tiny.client.tasks('7'),
      (error) => error instanceof nc.NativeProtocolError,
      'generic body bound still applies',
    );
  });

  await test('blob metadata route: 1 resolves, 0 is a 404, several are the typed 409 conflict', async () => {
    const digest = 'd'.repeat(64);
    const emptyDigest = 'e'.repeat(64);
    const conflictDigest = 'f'.repeat(64);
    const { client, calls } = makeClient({
      [`GET /native/session/7/attachments/blob/${digest}`]: () => jsonResponse(attachmentRefJson),
      [`GET /native/session/7/attachments/blob/${emptyDigest}`]: () =>
        jsonResponse(
          {
            error: {
              code: 'not_found',
              message: `attachment blob ${emptyDigest} in session 7`,
              retryable: false,
            },
          },
          404,
        ),
      // The two-refs-same-digest fixture: the daemon never picks a winner;
      // the 409 lists both candidate ref ids and the client surfaces it.
      [`GET /native/session/7/attachments/blob/${conflictDigest}`]: () =>
        jsonResponse(
          {
            error: {
              code: 'conflict',
              message:
                `attachment digest ${conflictDigest} in session 7 has 2 references; ` +
                'resolve one by ref_id: [11, 12]',
              retryable: false,
            },
          },
          409,
        ),
    });
    assertDeepEqual(
      await client.attachmentBlobReference('7', digest),
      attachmentRefJson,
      'exactly one reference resolves its own metadata',
    );
    findCall(calls, 'GET', `/native/session/7/attachments/blob/${digest}`);
    await assertRejects(
      () => client.attachmentBlobReference('7', emptyDigest),
      (error) =>
        error instanceof nc.NativeApiError && error.status === 404 && error.code === 'not_found',
      'zero references',
    );
    await assertRejects(
      () => client.attachmentBlobReference('7', conflictDigest),
      (error) =>
        error instanceof nc.NativeApiError &&
        error.status === 409 &&
        error.code === 'conflict' &&
        error.retryable === false &&
        error.message.includes('11') &&
        error.message.includes('12'),
      'several references',
    );
    assertProtocol(
      () => client.attachmentBlobReference('7', 'not-a-digest'),
      'digest must be 64 lowercase hex chars',
    );
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

async function permissionReplyTests() {
  await test('pending permissions parse with the OWNING session and free-form detail text', () => {
    const list = nc.validatePermissionList({
      permissions: [
        { id: '7', session_id: '9', capability: 'shell', detail: { tool: 'bash' } },
        { id: '8', session_id: '7', capability: 'write_file', detail: 'write a.ts' },
      ],
    });
    assertEqual(list.length, 2);
    assertEqual(list[0].id, '7');
    assertEqual(list[0].sessionId, '9');
    assertEqual(list[0].capability, 'shell');
    assertEqual(list[0].detail, '{"tool":"bash"}', 'object detail must be kept as text');
    assertEqual(list[1].sessionId, '7');
    assertEqual(list[1].detail, 'write a.ts');
  });

  await test('permission reply sends the strict session-scoped body and accepts {ok:true}', async () => {
    const { client, calls } = makeClient({
      'POST /native/permission/reply': () => jsonResponse({ ok: true }),
    });
    const ack = await client.replyPermission('9', '7', 'allow');
    assertEqual(ack.ok, true);
    assertDeepEqual(findCall(calls, 'POST', '/native/permission/reply').body, {
      session_id: '9',
      permission_id: '7',
      decision: 'allow',
    });
    assertEqual(calls.filter((call) => call.path === '/native/permission/reply').length, 1);
  });

  await test('unknown/expired id: the typed 409 conflict is surfaced and never retried', async () => {
    const { client, calls } = makeClient({
      'POST /native/permission/reply': () =>
        new Response(
          JSON.stringify({
            error: {
              code: 'conflict',
              message: 'permission 7 unknown or already resolved',
              retryable: false,
            },
          }),
          { status: 409 },
        ),
    });
    let refused = null;
    await assertRejects(
      () => client.replyPermission('9', '7', 'allow'),
      (error) => {
        refused = error;
        return (
          error instanceof nc.NativeApiError &&
          error.status === 409 &&
          error.code === 'conflict' &&
          error.retryable === false
        );
      },
      'typed conflict refusal',
    );
    assertEqual(nc.classifyPermissionReplyFailure(refused), 'unknown_or_resolved');
    assert(
      nc.permissionReplyFailureMessage('unknown_or_resolved', '7').includes('already resolved'),
      'the surfaced state must name the resolution authority',
    );
    assertEqual(
      calls.filter((call) => call.path === '/native/permission/reply').length,
      1,
      'a typed 409 must never be retried blindly',
    );
  });

  await test('foreign session: the typed 409 permission_session_mismatch is surfaced and never retried', async () => {
    const { client, calls } = makeClient({
      'POST /native/permission/reply': () =>
        new Response(
          JSON.stringify({
            error: {
              code: 'permission_session_mismatch',
              message: 'permission 7 is owned by session 8, not session 9',
              retryable: false,
            },
          }),
          { status: 409 },
        ),
    });
    let refused = null;
    await assertRejects(
      () => client.replyPermission('9', '7', 'deny'),
      (error) => {
        refused = error;
        return (
          error instanceof nc.NativeApiError &&
          error.status === 409 &&
          error.code === 'permission_session_mismatch' &&
          error.retryable === false
        );
      },
      'typed session-mismatch refusal',
    );
    assertEqual(nc.classifyPermissionReplyFailure(refused), 'session_mismatch');
    assert(
      nc.permissionReplyFailureMessage('session_mismatch', '7').includes('different session'),
      'the surfaced state must name the ownership conflict',
    );
    assertEqual(
      calls.filter((call) => call.path === '/native/permission/reply').length,
      1,
      'a session mismatch must never be retried blindly',
    );
  });

  await test('unrelated API errors are never classified as permission-reply refusals', () => {
    assertEqual(
      nc.classifyPermissionReplyFailure(new nc.NativeApiError(401, 'unauthorized', 'nope', false)),
      null,
    );
    assertEqual(
      nc.classifyPermissionReplyFailure(new nc.NativeApiError(409, 'shadow_unregistered', 'no shadow', false)),
      null,
      'other 409 codes stay plain API errors',
    );
    assertEqual(nc.classifyPermissionReplyFailure(new Error('transport down')), null);
    assertEqual(nc.classifyPermissionReplyFailure(undefined), null);
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
      hidden: false,
      value: '',
      type: '',
      disabled: false,
      checked: false,
      offsetTop: 0,
      // Entries occupy layout in the real panel; the fake gives every
      // article a fixed height so anchor preservation is measurable.
      offsetHeight: tagName === 'article' ? 40 : 0,
      clientHeight: 0,
    };
    Object.defineProperty(node, 'scrollHeight', {
      get: () =>
        Math.max(
          node.clientHeight,
          node.children.reduce((sum, child) => sum + (child.offsetHeight || 0), 0),
        ),
    });
    node.appendChild = (child) => {
      child.offsetTop = node.children.reduce(
        (sum, entry) => sum + (entry.offsetHeight || 0),
        0,
      );
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
    querySelectorAll(selector) {
      const match = /^\[data-evidence="([^"]+)"\]$/.exec(String(selector));
      if (!match) {
        return [];
      }
      const found = [];
      for (const root of nodesById.values()) {
        walkFake(root, (node) => {
          if (node.attributes['data-evidence'] === match[1]) {
            found.push(node);
          }
        });
      }
      return found;
    },
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
  // The real webview loads the composer draft policy BEFORE chat.js (see
  // src/webview.ts); the fake context mirrors that exact script order so the
  // draft-clearing contract is exercised, not the fallback.
  const composerSource = readFileSync(
    new URL('../media/composer-state.js', import.meta.url),
    'utf8',
  );
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
  vm.runInContext(composerSource, sandbox);
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

// ------------------- submission single-flight + verification authority + scroll

function submissionEnvelope(overrides = {}) {
  return {
    text: 'ship it',
    sessionId: null,
    draftId: null,
    messageId: null,
    files: [],
    attachments: [],
    ...overrides,
  };
}

/**
 * A fake host + daemon around the REAL `TaskStartGate` and admission path.
 * The daemon persists the receipt BEFORE the injected failure, so a lost
 * response has a durable original to dedupe the retry against.
 */
function fakeStartHost() {
  const gate = new ts.TaskStartGate();
  const calls = [];
  const receipts = new Map();
  let nextRun = 0;
  let idCounter = 0;
  let failNextStart = null;
  const newId = () => {
    idCounter += 1;
    return `00000000-0000-4000-8000-${String(idCounter).padStart(12, '0')}`;
  };
  const client = {
    uploadAttachment: async (sessionId, request) => ({
      ref_id: 1,
      digest: 'd'.repeat(64),
      mime: request.mime,
      filename: request.filename ?? null,
      size: Buffer.from(request.data_base64, 'base64').byteLength,
    }),
    startTaskRun: async (sessionId, request) => {
      calls.push({ sessionId, request: clone(request) });
      const id = request.submission_id;
      if (typeof id === 'string' && receipts.has(id)) {
        return receipts.get(id);
      }
      nextRun += 1;
      const receipt = { task_id: 1, run_id: `run-${nextRun}`, state: 'Pending' };
      if (typeof id === 'string') {
        receipts.set(id, receipt);
      }
      if (failNextStart !== null) {
        const error = failNextStart;
        failNextStart = null;
        throw error;
      }
      return receipt;
    },
  };
  const run = async (envelope, contract = null, submissionId = null) => {
    if (gate.inFlight()) {
      return { ignored: true };
    }
    const decision = gate.admit({
      submissionId,
      pending: envelope,
      files: envelope.files,
      contract,
      newId,
    });
    if (decision.action !== 'start') {
      return { ignored: true, reason: decision.action };
    }
    const outcome = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: decision.snapshot.pending,
      settings: {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        files: decision.snapshot.files,
        completionContract: decision.snapshot.contract,
        submissionId: decision.snapshot.submissionId,
      },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {},
    });
    gate.settle(
      decision.snapshot.submissionId,
      outcome.ok
        ? 'started'
        : ts.submissionRetryable(outcome.failure)
          ? 'transport'
          : 'refused',
    );
    return { outcome, submissionId: decision.snapshot.submissionId };
  };
  return {
    gate,
    calls,
    receipts,
    run,
    failNextStart: (error) => {
      failNextStart = error;
    },
  };
}

async function submissionSingleFlightTests() {
  await test('double click / Enter+click / triple submit admit exactly ONE start', async () => {
    for (const label of ['double click', 'Enter+click race', 'three rapid submits']) {
      const host = fakeStartHost();
      const count = label === 'three rapid submits' ? 3 : 2;
      const running = [];
      for (let i = 0; i < count; i += 1) {
        running.push(host.run(submissionEnvelope()));
      }
      const results = await Promise.all(running);
      assertEqual(host.calls.length, 1, `${label}: the daemon must see exactly one start`);
      assertEqual(results[0].outcome.ok, true, `${label}: the first submit starts`);
      for (const late of results.slice(1)) {
        assertEqual(late.ignored, true, `${label}: every later submit is ignored`);
      }
      assert(
        typeof host.calls[0].request.submission_id === 'string',
        `${label}: the start must carry a submission_id`,
      );
    }
  });

  await test('lost HTTP response + same-body retry reuses the id and the daemon returns one receipt', async () => {
    const host = fakeStartHost();
    host.failNextStart(new Error('socket closed after the daemon durably admitted'));
    const first = await host.run(submissionEnvelope());
    assertEqual(first.outcome.ok, false, 'the lost response is a failure');
    assertEqual(first.outcome.failure.kind, 'transport');
    assertEqual(host.calls.length, 1);
    // The explicit retry of the SAME immutable submission: same id, and the
    // daemon returns the original receipt instead of a second run.
    const retry = await host.run(submissionEnvelope());
    assertEqual(host.calls.length, 2);
    assertEqual(
      host.calls[0].request.submission_id,
      host.calls[1].request.submission_id,
      'the retry must reuse the same submission_id',
    );
    assertEqual(retry.outcome.ok, true);
    assertEqual(retry.outcome.runId, 'run-1', 'the original receipt is returned');
    assertEqual(host.receipts.size, 1, 'the daemon holds exactly one receipt');
  });

  await test('success and typed refusal clear the logical submission; transport keeps it retryable', async () => {
    // Success: the next identical body is a NEW logical submission.
    const success = fakeStartHost();
    const first = await success.run(submissionEnvelope());
    const second = await success.run(submissionEnvelope());
    assert(first.submissionId !== second.submissionId, 'success must end the submission');
    assertEqual(success.calls.length, 2);
    assert(
      success.calls[0].request.submission_id !== success.calls[1].request.submission_id,
      'a new logical start carries a new submission_id',
    );

    // Typed 4xx refusal: cleared as well.
    const refused = fakeStartHost();
    refused.failNextStart(new nc.NativeApiError(400, 'malformed', 'bad body', false));
    const refusal = await refused.run(submissionEnvelope());
    assertEqual(refusal.outcome.failure.kind, 'validation');
    const next = await refused.run(submissionEnvelope());
    assert(
      next.submissionId !== refusal.submissionId,
      'a typed refusal must end the submission',
    );

    // Explicit retry naming the pending id restarts the stored snapshot.
    const retry = fakeStartHost();
    retry.failNextStart(new Error('lost'));
    const pendingStart = await retry.run(submissionEnvelope());
    const explicit = await retry.run(
      submissionEnvelope(),
      null,
      pendingStart.submissionId,
    );
    assertEqual(retry.calls.length, 2);
    assertEqual(
      retry.calls[0].request.submission_id,
      retry.calls[1].request.submission_id,
      'an explicit same-id retry reuses the id',
    );
    assertEqual(explicit.submissionId, pendingStart.submissionId);

    // The daemon's typed "submission id still in flight" 409 is the
    // lost-response race: the id MUST be kept so the settled original
    // replays instead of a fresh id admitting a second durable run.
    const racing = fakeStartHost();
    racing.failNextStart(
      new nc.NativeApiError(
        409,
        'conflict',
        'task start with submission id "abc" is already in flight; retry once it settles',
        false,
      ),
    );
    const racingStart = await racing.run(submissionEnvelope());
    assertEqual(racingStart.outcome.ok, false);
    const settled = await racing.run(submissionEnvelope());
    assertEqual(racing.calls.length, 2);
    assertEqual(
      racing.calls[0].request.submission_id,
      racing.calls[1].request.submission_id,
      'an in-flight conflict must keep the submission id',
    );
    assertEqual(settled.outcome.ok, true);
    assertEqual(settled.outcome.runId, 'run-1', 'the settled original receipt replays');
  });

  await test('text typed and attachment changes while pending never join the pending body', async () => {
    const host = fakeStartHost();
    const first = host.run(submissionEnvelope({ text: 'goal A' }));
    const typed = host.run(submissionEnvelope({ text: 'goal B typed while pending' }));
    const [started, late] = await Promise.all([first, typed]);
    assertEqual(host.calls.length, 1, 'the pending start is the only daemon start');
    assertEqual(host.calls[0].request.goal, 'goal A', 'the pending body keeps its snapshot text');
    assertEqual(late.ignored, true);

    const withBytes = fakeStartHost();
    const original = submissionEnvelope({
      text: 'with bytes',
      attachments: [binaryAttachment()],
    });
    const changed = submissionEnvelope({
      text: 'with bytes',
      attachments: [binaryAttachment({ dataBase64: 'QUJDRA==', bytes: 4 })],
    });
    const [sent, ignored] = await Promise.all([
      withBytes.run(original),
      withBytes.run(changed),
    ]);
    assertEqual(withBytes.calls.length, 1, 'only the original attachment body is sent');
    assertEqual(withBytes.calls[0].request.attachments.length, 1);
    assertEqual(
      withBytes.calls[0].request.attachments[0].size,
      4,
      'the sent attachment body is the immutable original',
    );
    assertEqual(ignored.ignored, true);
    assertEqual(sent.outcome.ok, true);

    // After a transport failure, a CHANGED body is a NEW logical submission
    // (a changed attachment can never silently reuse the pending id).
    const retry = fakeStartHost();
    retry.failNextStart(new Error('lost'));
    const failedStart = await retry.run(original);
    const changedAfterFailure = await retry.run(changed);
    assert(
      changedAfterFailure.submissionId !== failedStart.submissionId,
      'changed content after a transport failure is a new logical submission',
    );

    // The completion contract and the workspace file list are part of the
    // retry identity too: changing either starts a new logical submission,
    // while an unchanged body retries the stored id.
    const identity = fakeStartHost();
    const contractA = { include_commit: false, include_push: false, include_pr: false };
    const contractB = { include_commit: true, include_push: false, include_pr: false };
    identity.failNextStart(new Error('lost'));
    const base = await identity.run(submissionEnvelope(), contractA);
    const otherContract = await identity.run(submissionEnvelope(), contractB);
    assert(
      otherContract.submissionId !== base.submissionId,
      'a changed completion contract is a new logical submission',
    );
    identity.failNextStart(new Error('lost'));
    const withFiles = await identity.run(
      submissionEnvelope({ files: ['src/a.ts'] }),
      contractB,
    );
    assert(
      withFiles.submissionId !== otherContract.submissionId,
      'a changed file list is a new logical submission',
    );
    const sameAgain = await identity.run(
      submissionEnvelope({ files: ['src/a.ts'] }),
      contractB,
    );
    assertEqual(
      sameAgain.submissionId,
      withFiles.submissionId,
      'an unchanged body retries the stored submission id',
    );
  });

  await test('a transport retry reuses the retained uploads under the same submission id', async () => {
    const retainer = new ts.PendingSubmissionRetainer();
    const uploads = [];
    const calls = [];
    const client = {
      uploadAttachment: async (sessionId, request) => {
        uploads.push(request);
        return {
          ref_id: uploads.length,
          digest: String(uploads.length).repeat(64),
          mime: request.mime,
          filename: request.filename ?? null,
          size: Buffer.from(request.data_base64, 'base64').byteLength,
        };
      },
      startTaskRun: async (sessionId, request) => {
        calls.push(clone(request));
        if (calls.length === 1) {
          throw new Error('lost after the upload');
        }
        return taskRunStartedJson;
      },
    };
    const parsed = ts.parsePendingSubmission({
      text: 'ship the screenshot',
      sessionId: '7',
      draftId: 'draft-1',
      messageId: 'msg-1',
      files: [],
      attachments: [binaryAttachment()],
    });
    assert(parsed !== null, 'the envelope fixture must parse');
    const first = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: retainer.restore(parsed),
      settings: {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        submissionId: '00000000-0000-4000-8000-000000000001',
      },
      onStarted: () => {},
      onFailure: () => {},
      restore: (failure, enriched) => retainer.retain(enriched),
    });
    assertEqual(first.ok, false, 'the first attempt fails transport');
    assertEqual(uploads.length, 1);
    // Handler-style retry: the stored snapshot + retained uploads merge back.
    const retryPending = retainer.restore(parsed);
    assert(
      retryPending.attachments[0].uploaded !== undefined,
      'the retained upload must merge back into the retry envelope',
    );
    const second = await ts.admitPendingSubmission({
      client,
      sessionId: '7',
      pending: retryPending,
      settings: {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        submissionId: '00000000-0000-4000-8000-000000000001',
      },
      onStarted: () => {},
      onFailure: () => {},
      restore: () => {},
    });
    assertEqual(second.ok, true);
    assertEqual(uploads.length, 1, 'the retry must not upload the same bytes twice');
    assertEqual(calls.length, 2);
    assertEqual(calls[0].submission_id, calls[1].submission_id);
  });

  await test('the request builder forwards only a canonical submission_id', () => {
    assertDeepEqual(
      ts.startTaskRequest('goal', {
        mutationMode: '',
        maxTokens: 0,
        maxCostMicro: 0n,
        submissionId: '00000000-0000-4000-8000-000000000001',
      }),
      { goal: 'goal', submission_id: '00000000-0000-4000-8000-000000000001' },
    );
    const junk = ts.startTaskRequest('goal', {
      mutationMode: '',
      maxTokens: 0,
      maxCostMicro: 0n,
      submissionId: 'not a submission id',
    });
    assert(!('submission_id' in junk), 'a non-canonical id must never reach the wire');
    assertEqual(
      ts.canonicalSubmissionId('ABCDEF00-0000-4000-8000-000000000001'),
      'abcdef00-0000-4000-8000-000000000001',
      'canonical ids normalize to lowercase hex',
    );
    assertEqual(ts.canonicalSubmissionId(''), null);
    assertEqual(ts.canonicalSubmissionId('x'.repeat(500)), null);
    // The retry classification: only failures that may have left a receipt
    // keep the id.
    assertEqual(ts.submissionRetryable({ status: null, message: 'socket closed' }), true);
    assertEqual(ts.submissionRetryable({ status: 500, message: 'internal' }), true);
    assertEqual(ts.submissionRetryable({ status: 408, message: 'timeout' }), true);
    assertEqual(ts.submissionRetryable({ status: 429, message: 'busy' }), true);
    assertEqual(
      ts.submissionRetryable({
        status: 409,
        message: 'task start with submission id "abc" is already in flight; retry once it settles',
      }),
      true,
    );
    assertEqual(
      ts.submissionRetryable({
        status: 409,
        message: 'submission id "abc" was already used for a different task start',
      }),
      false,
    );
    assertEqual(
      ts.submissionRetryable({ status: 409, message: 'session has no registered worktree row' }),
      false,
    );
    assertEqual(ts.submissionRetryable({ status: 400, message: 'malformed' }), false);
    assertEqual(ts.submissionRetryable({ status: 403, message: 'forbidden' }), false);
  });

  await test('failed fetch / key helpers: verification authority is identity, never counts', () => {
    const verification = {
      owed: [{ opId: 'op-1', tool: 'bash', startedMs: 1, status: 'pending', effectStatus: null }],
      failedChecks: [{ id: 'check-1', detail: 'cargo test failed' }],
    };
    const key = (over = {}) =>
      ts.verificationAuthorityKey({
        taskId: '42',
        state: 'running',
        revision: 'rev-1',
        verification,
        ...over,
      });
    const base = key();
    assertEqual(key(), base, 'identical authority inputs yield the identical key');
    // Same COUNTS, different failed-check identity.
    assert(
      key({
        verification: {
          ...verification,
          failedChecks: [{ id: 'check-2', detail: 'cargo test failed' }],
        },
      }) !== base,
      'a changed failed-check identity must invalidate the cache',
    );
    // Same counts, different evidence detail.
    assert(
      key({
        verification: {
          ...verification,
          failedChecks: [{ id: 'check-1', detail: 'cargo test failed elsewhere' }],
        },
      }) !== base,
      'changed evidence must invalidate the cache',
    );
    // Same counts, different owed op identity.
    assert(
      key({
        verification: {
          ...verification,
          owed: [{ ...verification.owed[0], opId: 'op-2' }],
        },
      }) !== base,
      'a changed owed op identity must invalidate the cache',
    );
    assert(key({ revision: 'rev-2' }) !== base, 'a task revision change invalidates');
    assert(key({ state: 'done' }) !== base, 'a task state change invalidates');
    assert(key({ taskId: '43' }) !== base, 'another task never reuses the cache');
  });

  await test('a failed fetch after an authority change never returns the previous view', () => {
    const viewA = { records: [{ recordId: 'A', criteria: [], checks: [] }] };
    const cache = new ts.EvidenceAuthorityCache();
    cache.store('authority-A', viewA);
    assertDeepEqual(cache.read('authority-A'), { view: viewA, unavailable: null });
    // A success -> B changed -> B fetch fails: B is explicitly unavailable
    // and the old A view is unreachable.
    cache.fail('authority-B', 'verification read failed: 503');
    assertDeepEqual(cache.read('authority-B'), {
      view: null,
      unavailable: 'verification read failed: 503',
    });
    assertEqual(cache.read('authority-A'), null, 'A must never render as B');
    // A same-authority transient failure is explicit, never silently verified.
    const fresh = new ts.EvidenceAuthorityCache();
    fresh.store('authority-A', viewA);
    fresh.fail('authority-A', 'transient network failure');
    assertDeepEqual(fresh.read('authority-A'), {
      view: null,
      unavailable: 'transient network failure',
    });
    // Recovery stores the fresh read again.
    fresh.store('authority-A', viewA);
    assertDeepEqual(fresh.read('authority-A'), { view: viewA, unavailable: null });
  });

  await test('architectural: no proof-like cache returns data after its authority key changed', () => {
    const source = readFileSync(new URL('../src/extension.ts', import.meta.url), 'utf8');
    const verificationStart = source.indexOf('async function taskVerificationFor(');
    assert(verificationStart >= 0, 'taskVerificationFor must exist');
    const verificationBody = source.slice(
      verificationStart,
      source.indexOf('\n}', verificationStart),
    );
    assert(
      !verificationBody.includes('return active.taskVerification'),
      'taskVerificationFor must never hand back the previous view',
    );
    assert(
      verificationBody.includes('taskVerificationCache.fail('),
      'a failed verification fetch must drop the cached view to explicit unavailable',
    );
    assert(
      verificationBody.includes('verificationAuthorityKey('),
      'the verification cache key must come from durable authority, not counts',
    );
    const proofStart = source.indexOf('async function taskProofFor(');
    assert(proofStart >= 0, 'taskProofFor must exist');
    const proofBody = source.slice(proofStart, source.indexOf('\n}', proofStart));
    assert(
      proofBody.includes('active.taskProof = null'),
      'the proof cache must drop its view on a failed read',
    );
    const library = readFileSync(new URL('../src/taskStart.ts', import.meta.url), 'utf8');
    assert(
      library.includes('class EvidenceAuthorityCache'),
      'the authority-keyed cache policy must live in the dependency-free module',
    );
  });
}

function transcriptEntry(seq, text, overrides = {}) {
  return {
    id: `m-${seq}`,
    role: seq % 2 === 0 ? 'assistant' : 'user',
    seq,
    createdMs: seq,
    text,
    reasoning: '',
    summary: '',
    tools: [],
    ...overrides,
  };
}

function transcriptSnapshot(entries) {
  return { ...webviewSnapshot([]), transcript: entries };
}

async function transcriptScrollTests() {
  await test('transcript scroll ownership: pinned follows, initial pins, no delta is a no-op', () => {
    const harness = runChatWebview(transcriptSnapshot([]));
    const container = harness.dom.document.getElementById('entries');
    container.clientHeight = 200;
    const entries = [transcriptEntry(1, 'one'), transcriptEntry(2, 'two')];
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    assertEqual(container.scrollTop, container.scrollHeight, 'initial load pins to the bottom');

    // At the bottom + message -> stays pinned.
    entries.push(transcriptEntry(3, 'three'));
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    assertEqual(container.scrollTop, container.scrollHeight, 'a pinned view follows the bottom');
    assertEqual(container.children.length, 3, 'the rebuilt transcript carries every entry');

    // 3px from the bottom (inside the slack) -> still pinned.
    container.scrollTop = container.scrollHeight - container.clientHeight - 3;
    entries.push(transcriptEntry(4, 'four'));
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    assertEqual(
      container.scrollTop,
      container.scrollHeight,
      '3px from the bottom is still pinned',
    );
  });

  await test('an unpinned reader keeps the first visible entry and pixel offset', () => {
    const harness = runChatWebview(transcriptSnapshot([]));
    const container = harness.dom.document.getElementById('entries');
    container.clientHeight = 200;
    const entries = [];
    for (let seq = 1; seq <= 20; seq += 1) {
      entries.push(transcriptEntry(seq, `message ${seq}`));
    }
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    // 500px above the bottom of an 800px transcript.
    container.scrollTop = 500;
    const anchorNode = container.children[12];
    const anchorOffset = 500 - anchorNode.offsetTop;
    entries.push(transcriptEntry(21, 'message 21'));
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    assertEqual(container.scrollTop, anchorNode.offsetTop + anchorOffset);
    assertEqual(container.scrollTop, 500, 'the reading position survives the rebuild');
    assert(container.scrollTop < container.scrollHeight - container.clientHeight);
  });

  await test('background refresh with no transcript delta mutates zero scroll and keeps evidence', () => {
    const tool = {
      toolCallId: 't-1',
      name: 'bash',
      state: 'done',
      input: null,
      excerpt: 'output',
      exitCode: 0,
      artifact: 'evidence:42',
    };
    const entries = [transcriptEntry(1, 'one', { tools: [tool] })];
    const harness = runChatWebview(transcriptSnapshot([]));
    const container = harness.dom.document.getElementById('entries');
    container.clientHeight = 200;
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    container.scrollTop = 0;
    harness.deliver({ type: 'evidence', id: 42, text: 'DETAIL', truncated: false });
    assert(
      fakeText(container).includes('DETAIL'),
      'the expanded evidence must render into its holder',
    );
    const before = container.children.slice();
    const scrollBefore = container.scrollTop;
    // The SAME transcript arrives from a background snapshot: no rebuild, no
    // scroll mutation, and the expanded evidence survives.
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    assertEqual(container.scrollTop, scrollBefore, 'no delta must not move the scroll');
    assertEqual(
      container.children.length,
      before.length,
      'no delta must not rebuild the transcript DOM',
    );
    for (let index = 0; index < before.length; index += 1) {
      assert(
        container.children[index] === before[index],
        'no delta must keep the exact rendered entry nodes',
      );
    }
    assert(fakeText(container).includes('DETAIL'), 'the expanded evidence survives the refresh');
  });

  await test('own submission explicitly pins the transcript', () => {
    const harness = runChatWebview(transcriptSnapshot([]));
    const container = harness.dom.document.getElementById('entries');
    container.clientHeight = 200;
    const entries = [];
    for (let seq = 1; seq <= 20; seq += 1) {
      entries.push(transcriptEntry(seq, `message ${seq}`));
    }
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    container.scrollTop = 500;
    // The user's own submission requests the pin; the next changed render
    // consumes it and jumps to the bottom.
    harness.dom.document.getElementById('goal').value = 'my new goal';
    harness.dom.document
      .getElementById('composer')
      .dispatch('submit', { preventDefault() {} });
    entries.push(transcriptEntry(21, 'the user message'));
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    assertEqual(container.scrollTop, container.scrollHeight, 'own submission pins to the bottom');
    // The one-shot pin was consumed: a later changed render respects the
    // reading position again.
    container.scrollTop = 500;
    entries.push(transcriptEntry(22, 'assistant reply'));
    harness.deliver({ type: 'snapshot', snapshot: transcriptSnapshot(entries) });
    assert(container.scrollTop < container.scrollHeight - container.clientHeight);
  });

  await test('the composer is single-flight: submit locks controls and only an explicit result releases', () => {
    const { posted, dom, deliver } = runChatWebview(webviewSnapshot([]));
    const goal = dom.document.getElementById('goal');
    const composer = dom.document.getElementById('composer');
    const send = dom.document.getElementById('btn-send');
    const newTask = dom.document.getElementById('btn-new-task');
    goal.value = 'first goal';
    composer.dispatch('submit', { preventDefault() {} });
    // posted[0] is the initial ready message.
    assertEqual(posted.length, 2, 'the first submit posts exactly one sendGoal');
    assertDeepEqual(posted[1], { type: 'sendGoal', goal: 'first goal' });
    assertEqual(send.disabled, true, 'Run task is disabled while submitting');
    assertEqual(newTask.disabled, true, 'New task is disabled while submitting');
    assertEqual(
      dom.document.getElementById('contract-commit').disabled,
      true,
      'contract mutation is disabled while submitting',
    );
    // Text typed while pending and a second submit never join the request.
    goal.value = 'second goal typed while pending';
    composer.dispatch('submit', { preventDefault() {} });
    assertEqual(posted.length, 2, 'a second submit while pending must not post');
    assert(!('completionContract' in posted[1]), 'the pending message body is immutable');
    // An explicit failure releases the lock and preserves draft + contract.
    dom.document.getElementById('contract-commit').checked = true;
    deliver({ type: 'startResult', goal: 'first goal', ok: false });
    assertEqual(send.disabled, false, 'a failure re-enables Run task');
    assertEqual(newTask.disabled, false, 'a failure re-enables New task');
    assertEqual(
      goal.value,
      'second goal typed while pending',
      'the draft typed while pending is preserved on failure',
    );
    assertEqual(dom.document.getElementById('contract-commit').checked, true, 'contract kept for retry');
    assertEqual(dom.document.getElementById('contract-commit').disabled, false);
    // The next submit is a new logical submission with the current body.
    composer.dispatch('submit', { preventDefault() {} });
    assertEqual(posted.length, 3);
    assertEqual(posted[2].goal, 'second goal typed while pending');
    assertDeepEqual(posted[2].completionContract, {
      include_commit: true,
      include_push: false,
      include_pr: false,
    });
    deliver({ type: 'startResult', goal: 'second goal typed while pending', ok: true });
    assertEqual(goal.value, '', 'a success clears the unchanged draft');
    assertEqual(send.disabled, false, 'a success leaves the composer enabled');
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
  await attachmentHttpContractTests();
  await boardAndForwardingTests();
  await permissionReplyTests();
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
  await submissionSingleFlightTests();
  await transcriptScrollTests();
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
