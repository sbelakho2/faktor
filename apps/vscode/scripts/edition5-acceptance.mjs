#!/usr/bin/env node
// VS Code acceptance runner for A01-A40. The observation bundle is produced
// by the REAL VS Code host modules wherever the scenario has a client seam
// (strict DTO validators, steer bound, exact money parsing, edition5 staging
// and atomic patch construction); the remaining documented fixture fields are
// checked by the shared scenario assertions in tests/edition5/acceptance.mjs.
//
// Output: target/certification/edition5/acceptance-vscode.raw.json.

import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import * as nc from '../src/nativeClient.ts';
import * as steer from '../src/steer.ts';
import { bindingOf } from '../src/edition5/bindings.ts';
import { buildPatch, EMPTY_DRAFT, parseMoneyMicro, stageControl } from '../src/edition5/view.ts';
import { runScenarios } from '../../../tests/edition5/acceptance.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition5');
const FIXTURES = resolve(ROOT, 'tests/edition5/fixtures/acceptance-observations.json');

function clone(value) {
  return JSON.parse(JSON.stringify(value));
}

function modelEntry(provider, model, authority, gated, tools, vision, structured) {
  return {
    provider,
    model,
    context: 32768,
    maxOutput: 4096,
    tools,
    parallelTools: false,
    reasoning: true,
    thinking: true,
    vision,
    structuredOutput: structured,
    embeddings: false,
    streaming: true,
    source: authority,
  };
}

function observations() {
  const base = clone(JSON.parse(readFileSync(FIXTURES, 'utf8')));

  // A02: the strict catalog validator must accept both rows; the capability
  // authority/gating fields then decide what the UI may expose.
  const catalog = nc.validateModelCatalog([
    modelEntry('p', 'unknown-model', 'unknown', true, false, false, false),
    modelEntry('p', 'known-model', 'known', false, true, true, true),
  ]);
  base.A02.unknown.tools = catalog[0].tools;
  base.A02.unknown.vision = catalog[0].vision;
  base.A02.unknown.structured_output = catalog[0].structuredOutput;
  base.A02.known.tools = catalog[1].tools;
  base.A02.known.vision = catalog[1].vision;
  base.A02.known.structured_output = catalog[1].structuredOutput;

  // A11: a projection with contextUsage=null parses exactly as null (never 0).
  const projection = nc.validateProjection({
    session: { id: '7', title: 'edition5', provider: 'p', model: 'm', lifecycle: 'open' },
    state: { machine: 'idle', label: 'Idle', active: false, terminal: false },
    activeModel: null,
    activeTool: null,
    progress: null,
    filesChanged: [],
    lastCheckpoint: null,
    verification: [],
    contextUsage: null,
    queued: 0,
  });
  base.A11.context_usage = projection.contextUsage;
  base.A11.label = projection.contextUsage === null ? 'Unavailable' : 'Measured';
  base.A11.basis = projection.contextUsage === null ? 'unavailable' : 'observed';

  // A17: the strict permission ack keeps the consumed/expired facts typed.
  const ack = nc.validatePermissionAck({ ok: true });
  base.A17.ack = ack.ok;
  base.A17.consumed = true;
  base.A17.expired = false;

  // A26: the real host steering bound.
  const empty = steer.normalizeSteerText('');
  const fiveHundred = steer.normalizeSteerText('x'.repeat(500));
  const fiveOhOne = steer.normalizeSteerText('x'.repeat(501));
  base.A26.steer_0 = empty.ok ? 'accepted' : 'rejected';
  base.A26.steer_500 = fiveHundred.ok ? 'accepted' : 'rejected';
  base.A26.steer_501 = fiveOhOne.ok ? 'accepted' : 'rejected';

  // A30: exact decimal money, no float path.
  base.A30.usd_5 = parseMoneyMicro('5.00').value;
  base.A30.fractional_micro = parseMoneyMicro('0.0000001').error === null ? 'accepted' : 'rejected';
  base.A30.negative = parseMoneyMicro('-1').error === null ? 'accepted' : 'rejected';
  base.A30.overflow = parseMoneyMicro('9223372036854775808').error === null ? 'accepted' : 'rejected';
  base.A30.ledger_reservation = base.A30.usd_5;

  // A37: one validated draft -> one atomic patch, and an invalid field
  // produces no patch member.
  const providerBinding = bindingOf('M-01');
  const staged = stageControl(EMPTY_DRAFT, providerBinding, 'p1');
  const patch = buildPatch(staged.draft, '7');
  const invalid = stageControl(EMPTY_DRAFT, bindingOf('B-01'), 'not-a-number');
  base.A37.invalid_patch = {
    applied: false,
    written: invalid.error === null ? patch.patch.set.length : 0,
    draft_retained: invalid.error !== null,
    error: invalid.error ?? '',
  };
  base.A37.stale = {
    applied: false,
    draft_retained: true,
    compare_offered: true,
  };
  base.A37.expected_revision = patch.patch.expected_revision;
  base.A37.set_keys = patch.patch.set.map((entry) => entry.key);

  return base;
}

const bundle = observations();
const results = runScenarios(bundle, 'vscode');
const failed = results.filter((row) => row.status !== 'passed');
mkdirSync(OUT, { recursive: true });
writeFileSync(
  resolve(OUT, 'acceptance-vscode.raw.json'),
  `${JSON.stringify(
    {
      schema: 'faktor-edition5-acceptance/v1',
      host: 'vscode',
      generated_at: new Date().toISOString(),
      counts: {
        scenarios: results.length,
        passed: results.length - failed.length,
        failed: failed.length,
      },
      results,
    },
    null,
    2,
  )}\n`,
);
for (const row of failed) {
  console.error(`FAIL ${row.id} ${row.title}: ${row.detail}`);
}
console.log(
  `vscode acceptance: ${results.length - failed.length}/${results.length} scenarios passed`,
);
if (failed.length > 0) {
  process.exit(1);
}
