#!/usr/bin/env node
// Surface discovery for the test programme.
//
// Closed over the REAL product tree: native routes, builtin tools, CLI
// command tree, provider kinds, VS Code/JetBrains contributions are taken
// from scripts/discover-capabilities.mjs (the same scanner the release
// certificate uses). The programme adds the report's other surface classes:
// controls, screens, workflows, acceptance scenarios, invariants, config keys
// and external adapters. A surface added anywhere in the tree therefore
// arrives as a new registry row; it cannot be invisible.

import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  discover,
  discoverCliCommands,
  discoverNativeRoutes,
  serverRouteFiles,
} from '../../scripts/discover-capabilities.mjs';
import { buildRegistry as buildEdition8Registry } from '../edition8/registry.mjs';
import { ADVERSARIAL_BY_KIND } from './model.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

export const REV8_ROUTE_IDS = Object.freeze([
  'native.delete.native.session.id',
  'native.delete.native.session.id.review.comments.comment',
  'native.get.native.mcp.servers.id.auth',
  'native.get.native.presets.id.fallback',
  'native.get.native.presets.id.roles',
  'native.get.native.session.id.review.comments',
  'native.get.native.session.id.settings',
  'native.patch.native.session.id.review.comments.comment',
  'native.post.native.catalog.probe',
  'native.post.native.mcp.servers.id.auth',
  'native.post.native.presets.id.fallback',
  'native.post.native.presets.id.roles',
  'native.post.native.session.id.changes.accept',
  'native.post.native.session.id.checkpoints.sequence.restore.preview',
  'native.post.native.session.id.export',
  'native.post.native.session.id.review.comments',
  'native.put.native.session.id.settings',
]);

const REV8 = new Set(REV8_ROUTE_IDS);

const REV8_TEST = 'crates/server/src/native/foundation_rev8_contracts_tests.rs';

const CREDENTIAL_ROUTES = Object.freeze([
  'native.get.native.providers.credentials',
  'native.post.native.providers.credentials',
  'native.post.native.providers.credentials.rotate',
  'native.delete.native.providers.credentials.handle',
]);

const ADAPTERS = Object.freeze([
  {
    id: 'adapter.github-app',
    path: 'crates/scm/src',
    test_ids: ['scm.webhook_signature_and_replay', 'scm.installation_scope'],
    layers: ['unit-property', 'contracts'],
    lanes: ['pull', 'trusted', 'weekly-live'],
    evidence_kind: 'recorded-real-world',
    oracle:
      'A forged or replayed webhook is refused; a genuine delivery lands once and installation ids never cross repositories.',
  },
  {
    id: 'adapter.kiwi-verifier',
    path: 'crates/verify/src',
    test_ids: ['verify.kiwi_optional_adapter', 'verify.forged_verdict_refused'],
    layers: ['unit-property', 'contracts'],
    lanes: ['pull', 'trusted', 'weekly-live'],
    evidence_kind: 'reference-implementation',
    oracle:
      'A missing required verifier is refused typed, never substituted; a verdict without evidence fails, and the native baseline is always present.',
  },
  {
    id: 'adapter.tangerine-intelligence',
    path: 'crates/orchestrator/src',
    test_ids: ['intelligence.tier_bounds', 'intelligence.hostile_findings'],
    layers: ['unit-property', 'contracts', 'agent-evals'],
    lanes: ['pull', 'trusted', 'weekly-live'],
    evidence_kind: 'property',
    oracle:
      'Tiers stay 0-5, findings are validated as data, and an absent or slow provider leaves the native contract byte-identical.',
  },
  {
    id: 'adapter.marketplace-vendors',
    path: 'crates/commerce/src',
    test_ids: ['commerce.vendor_fixture_drift', 'commerce.live_basket_canary'],
    layers: ['unit-property', 'contracts'],
    lanes: ['trusted', 'weekly-live'],
    evidence_kind: 'recorded-real-world',
    oracle:
      'Every mutated recorded vendor response yields drift or a correct parse; live canaries compare real basket totals.',
  },
  {
    id: 'adapter.mcp-transport',
    path: 'crates/mcp/src',
    test_ids: ['mcp.hostile_server_corpus', 'mcp.queue_bounds'],
    layers: ['unit-property', 'state-model-crash', 'contracts'],
    lanes: ['pull', 'trusted', 'weekly-live'],
    evidence_kind: 'property',
    oracle:
      'Garbage frames, hangs and crash loops are bounded and typed; a changed tool list never runs unchecked and shutdown leaves zero children.',
  },
  {
    id: 'adapter.sso-oidc',
    path: 'crates/cloud/src',
    test_ids: ['cloud.oidc_state_nonce_replay', 'cloud.role_route_matrix'],
    layers: ['unit-property', 'contracts'],
    lanes: ['pull', 'trusted', 'weekly-live'],
    evidence_kind: 'reference-implementation',
    oracle:
      'State/nonce replays and clock skew are refused; role x route cells allow or refuse exactly as the plan table says.',
  },
  {
    id: 'adapter.update-feed',
    path: 'crates/updater/src',
    test_ids: ['updater.manifest_signature', 'updater.downgrade_refused'],
    layers: ['unit-property', 'state-model-crash', 'contracts'],
    lanes: ['pull', 'trusted', 'weekly-live'],
    evidence_kind: 'reference-implementation',
    oracle:
      'Only signed manifests apply; a tampered package or disallowed downgrade is refused and rollback restores the exact old binary.',
  },
]);

function read(root, rel) {
  const path = join(root, rel);
  return existsSync(path) ? readFileSync(path, 'utf8') : '';
}

function walkFiles(root, rel, filter, out = []) {
  const dir = join(root, rel);
  if (!existsSync(dir)) return out;
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'target' || entry.name === 'node_modules' || entry.name === '.git') {
      continue;
    }
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walkFiles(root, relative(root, path), filter, out);
    else if (filter(path)) out.push(path);
  }
  return out;
}

/** Daemon config keys declared by the descriptor authority. */
export function discoverConfigKeys(root = ROOT) {
  const text = read(root, 'crates/foundation/src/config.rs');
  const keys = new Set();
  for (const match of text.matchAll(/"((?:faktor|native)\.[A-Za-z0-9_.-]+)"/g)) {
    keys.add(match[1]);
  }
  for (const match of text.matchAll(/Descriptor::new\(\s*"([^"]+)"/g)) {
    if (match[1] !== '*') keys.add(match[1]);
  }
  return [...keys].sort();
}

/** VS Code configuration properties (the host's own settings surface). */
export function discoverVsCodeConfigKeys(root = ROOT) {
  const path = join(root, 'apps/vscode/package.json');
  if (!existsSync(path)) return [];
  let document;
  try {
    document = JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return [];
  }
  const configuration = document?.contributes?.configuration ?? {};
  const sections = Array.isArray(configuration) ? configuration : [configuration];
  const keys = new Set();
  for (const section of sections) {
    for (const key of Object.keys(section?.properties ?? {})) keys.add(key);
  }
  return [...keys].sort();
}

function baseRows(root) {
  const discovered = discover(root);
  const routes = new Map(discoverNativeRoutes(root, serverRouteFiles(root)).map((route) => [route.id, route.file]));
  const rows = [];
  for (const id of discovered.ids) {
    if (id.startsWith('native.')) {
      const rev8 = REV8.has(id);
      const credential = CREDENTIAL_ROUTES.includes(id);
      const file = routes.get(id) ?? 'crates/server/src';
      const own = [];
      if (credential) own.push(`native.credential-contract.${id}`);
      if (rev8) own.push(`native.rev8-contract.${id}`);
      own.push(`native.route-contract.${id}`);
      rows.push({
        id,
        kind: 'native-route',
        path: file,
        layers: credential || rev8 ? ['contracts', 'state-model-crash'] : ['contracts'],
        lanes: credential || rev8 ? ['pull', 'trusted', 'nightly'] : ['pull', 'trusted'],
        test_ids: own,
        evidence_kind: 'exact-value',
        oracle:
          'Authenticated malformed/boundary requests return the typed refusal (never 5xx or a panic); auth is refused before the effect; the route is present at its exact method+path.',
        status: 'deferred',
        meta: {
          rev8,
          credential,
          method: id.split('.')[1],
          surface_path: id.slice(id.indexOf('.', id.indexOf('.') + 1) + 1).replaceAll('.', '/'),
        },
      });
    } else if (id.startsWith('cli.')) {
      const command = id.slice('cli.'.length);
      rows.push({
        id,
        kind: 'cli-command',
        path: 'crates/cli/src/main.rs',
        layers: ['contracts'],
        lanes: ['pull', 'trusted'],
        test_ids: [`cli.help.${command}`, `cli.exit-codes.${command}`],
        evidence_kind: 'exact-value',
        oracle:
          'Golden help resolves the exact command; success/usage/refusal exit codes are distinct and unknown flags fail rather than being ignored.',
        status: 'deferred',
        meta: { command },
      });
    } else if (id.startsWith('vscode.')) {
      rows.push({
        id,
        kind: id.includes('configuration') ? 'vscode-setting' : 'vscode-contribution',
        path: 'apps/vscode/package.json',
        layers: ['ide-screen-readers'],
        lanes: ['pull', 'trusted'],
        test_ids: [`vscode.contract.${id}`],
        evidence_kind: 'exact-value',
        oracle:
          'The contribution/setting exists with its declared id, is reachable from the packaged extension, and its effect is exercised against the daemon (not only the name).',
        status: 'deferred',
        meta: {},
      });
    } else if (id.startsWith('jetbrains.')) {
      rows.push({
        id,
        kind: 'jetbrains-contribution',
        path: 'apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml',
        layers: ['ide-screen-readers'],
        lanes: ['pull', 'trusted'],
        test_ids: [`jetbrains.contract.${id}`],
        evidence_kind: 'exact-value',
        oracle:
          'The action/tool window is registered, reachable from the palette, and its effect is asserted against the daemon.',
        status: 'deferred',
        meta: {},
      });
    } else if (id.startsWith('tool.')) {
      const tool = id.slice('tool.'.length);
      rows.push({
        id,
        kind: 'builtin-tool',
        path: 'crates/cli/src',
        layers: ['unit-property', 'state-model-crash', 'contracts'],
        lanes: ['pull', 'trusted'],
        test_ids: [`tool.contract.${tool}`, `tool.bad-args.${tool}`, `tool.bounds.${tool}`],
        evidence_kind: 'exact-value',
        oracle:
          'Bad arguments are refused before any effect; permission/sandbox gates precede it; output is bounded and treated as data; an unknown effect is never re-run.',
        status: 'stub',
        meta: { tool },
      });
    } else if (id.startsWith('provider.')) {
      const provider = id.slice('provider.'.length);
      rows.push({
        id,
        kind: 'provider-adapter',
        path: 'crates/provider/src',
        layers: ['unit-property', 'contracts'],
        lanes: ['pull', 'trusted', 'weekly-live'],
        test_ids: [`provider.recorded.${provider}`, `provider.normalize.${provider}`, `provider.live.${provider}`],
        evidence_kind: 'recorded-real-world',
        oracle:
          'Recorded split frames, split tool-call arguments and every error status normalize to the same event sequence as the reference; unsupported capabilities are refused before sending.',
        status: 'deferred',
        meta: { provider },
      });
    }
  }
  return rows;
}

function edition8Rows(root) {
  const registry = buildEdition8Registry(root);
  const rows = [];
  for (const control of registry.controls) {
    rows.push({
      id: `control.${control.id}`,
      kind: 'control',
      path: `docs/specs/edition8/controls.json#${control.id}`,
      layers: ['ide-screen-readers'],
      lanes: ['trusted', 'nightly', 'weekly-live'],
      test_ids: [
        `control.exercise.${control.id}`,
        `control.contract.${control.id}`,
        `control.visual.${control.id}`,
      ],
      evidence_kind: 'state-model',
      oracle:
        'Default/scope/apply timing from the contract; empty/loading/error/disabled/stale states render and recover; keyboard and screen-reader semantics match in both IDEs at 200% zoom and 320 px.',
      status: 'deferred',
      meta: { category: control.category_code, hosts: control.hosts },
    });
  }
  for (const screen of registry.screens) {
    rows.push({
      id: `screen.${screen.id}`,
      kind: 'screen',
      path: `docs/specs/edition8/screens.md#${screen.id}`,
      layers: ['ide-screen-readers'],
      lanes: ['trusted', 'nightly', 'weekly-live'],
      test_ids: [`screen.visual.${screen.id}`, `screen.contract.${screen.id}`],
      evidence_kind: 'exact-value',
      oracle:
        'Visual contract per IDE/theme/width with a reviewed baseline; primary action present; focus order stable; no fabricated values in empty or stale states.',
      status: 'deferred',
      meta: { controls: screen.controls },
    });
  }
  for (const workflow of registry.workflows) {
    rows.push({
      id: `workflow.${workflow.id}`,
      kind: 'workflow',
      path: `docs/specs/edition8/flows.md#${workflow.id}`,
      layers: ['ide-screen-readers', 'state-model-crash'],
      lanes: ['trusted', 'nightly', 'weekly-live'],
      test_ids: [`workflow.cells.${workflow.id}`, `workflow.recovery.${workflow.id}`],
      evidence_kind: 'state-model',
      oracle:
        'Pointer/keyboard/screen-reader/compact/recovery cells complete in each IDE, plus a daemon crash and network loss mid-flow; a forbidden state fails the run.',
      status: 'deferred',
      meta: { screens: workflow.screens },
    });
  }
  for (const scenario of registry.acceptance) {
    rows.push({
      id: `acceptance.${scenario.id}`,
      kind: 'acceptance-scenario',
      path: `docs/specs/edition8/acceptance.md#${scenario.id}`,
      layers: ['ide-screen-readers', 'state-model-crash'],
      lanes: ['trusted', 'nightly', 'weekly-live'],
      test_ids: [`acceptance.case.${scenario.id}`, `acceptance.adversarial.${scenario.id}`],
      evidence_kind: 'state-model',
      oracle:
        'Printed requirement, field values and forbidden-state scan pass, plus one adversarial variant; a stale or fabricated state fails.',
      status: 'deferred',
      meta: { hosts: scenario.hosts },
    });
  }
  return rows;
}

function invariantRows(root) {
  const text = read(root, 'tests/invariants.toml');
  const rows = [];
  for (const block of text.split('[[invariant]]').slice(1)) {
    const id = /^id\s*=\s*"(.*)"$/m.exec(block);
    if (!id) continue;
    const unit = /^unit_test\s*=\s*"(.*)"$/m.exec(block);
    const wiring = /^production_wiring\s*=\s*"(.*)"$/m.exec(block);
    const mutation = /^mutation_command\s*=\s*"(.*)"$/m.exec(block);
    const tests = [];
    if (unit?.[1]) tests.push(unit[1]);
    if (wiring?.[1]) tests.push(wiring[1]);
    if (tests.length === 0) tests.push(`script.${id[1]}`);
    rows.push({
      id: `invariant.${id[1]}`,
      kind: 'invariant',
      path: 'tests/invariants.toml',
      layers: ['unit-property', 'state-model-crash'],
      lanes: ['pull', 'trusted', 'nightly'],
      test_ids: tests.length > 0 ? tests : [`invariant.${id[1]}.mutation`],
      evidence_kind: 'property',
      oracle:
        'The unit test proves the statement, the wiring test proves production reaches it, and a planted mutation makes the oracle fire.',
      status: 'deferred',
      meta: {
        mutation: mutation?.[1] ?? '',
        unit_test: unit?.[1] ?? '',
        wiring_test: wiring?.[1] ?? '',
      },
    });
  }
  return rows;
}

function configRows(root) {
  const rows = [];
  for (const key of discoverConfigKeys(root)) {
    rows.push({
      id: `config.${key}`,
      kind: 'config-key',
      path: 'crates/foundation/src/config.rs',
      layers: ['unit-property', 'contracts'],
      lanes: ['pull', 'trusted'],
      test_ids: [`config.default.${key}`, `config.invalid.${key}`, `config.provenance.${key}`],
      evidence_kind: 'exact-value',
      oracle:
        'Default and winning layer are asserted; each invalid value is a typed error; unknown keys fail at start; secrets never appear in plain config.',
      status: 'stub',
      meta: { key },
    });
  }
  return rows;
}

function adapterRows(root) {
  const rows = [];
  const covered = new Set();
  const coveredDirs = new Set();
  for (const adapter of ADAPTERS) {
    if (!existsSync(join(root, adapter.path))) continue;
    covered.add(adapter.path);
    coveredDirs.add(adapter.path.split('/').slice(0, 2).join('/'));
    rows.push({
      id: adapter.id,
      kind: 'adapter',
      path: adapter.path,
      layers: [...adapter.layers],
      lanes: [...adapter.lanes],
      test_ids: [...adapter.test_ids],
      evidence_kind: adapter.evidence_kind,
      oracle: adapter.oracle,
      status: 'deferred',
      meta: {},
    });
  }
  // New adapter crates are discovered, not enumerated: any non-provider
  // crate whose name denotes an external integration gets its own row so a
  // new vendor/MCP/SCM/cloud adapter cannot be invisible.
  const cratesDir = join(root, 'crates');
  if (existsSync(cratesDir)) {
    for (const entry of readdirSync(cratesDir, { withFileTypes: true })) {
      if (!entry.isDirectory() || covered.has(`crates/${entry.name}`) || coveredDirs.has(`crates/${entry.name}`)) continue;
      const manifest = join(cratesDir, entry.name, 'Cargo.toml');
      if (!existsSync(manifest)) continue;
      const text = readFileSync(manifest, 'utf8');
      const name = /\[package\][\s\S]*?^name\s*=\s*"([^"]+)"/m.exec(text)?.[1] ?? entry.name;
      if (!/(adapter|integration|mcp|scm|cloud|updater|commerce|verify)/.test(name)) continue;
      if (name === 'faktor-provider') continue;
      rows.push({
        id: `adapter.${name.replace(/^faktor-/, '')}`,
        kind: 'adapter',
        path: `crates/${entry.name}`,
        layers: ['unit-property', 'contracts'],
        lanes: ['trusted', 'weekly-live'],
        test_ids: [`adapter.contract.${name}`],
        evidence_kind: 'recorded-real-world',
        oracle:
          'The adapter contract is asserted against recorded hostile responses and a live canary; absence or drift is typed, never silently skipped.',
        status: 'stub',
        meta: { discovered: true },
      });
    }
  }
  return rows;
}

function actNowRows() {
  return [
    {
      id: 'act-now.settlement-interleaving',
      kind: 'act-now',
      path: 'crates/agent/src/runtime/settlement.rs',
      layers: ['state-model-crash', 'unit-property'],
      lanes: ['pull', 'trusted', 'nightly'],
      test_ids: [
        'settlement_is_bound_to_the_admitted_task_id',
        'settlement_interleaving_at_every_tail_step_keeps_the_next_task_byte_identical',
        'settlement_modules_never_read_the_lazy_current_task_identity',
        'settlement-scan.lazy-task-id',
        'settlement-scan.await-points',
      ],
      evidence_kind: 'state-model',
      oracle:
        'Starting prompt N+1 at every settlement tail step leaves task N+1 byte-identical (goal, spend, state); the admitted identity is the only row written.',
      status: 'deferred',
      meta: { report_row: 1 },
    },
    {
      id: 'act-now.settlement-static-ban',
      kind: 'act-now',
      path: 'crates/agent/src/runtime/settlement.rs',
      layers: ['contracts'],
      lanes: ['pull', 'trusted'],
      test_ids: ['settlement-scan.lazy-task-id'],
      evidence_kind: 'static-scan',
      oracle:
        'No lazy task_id() read in a settlement write path: every settlement mutation threads the admitted TaskId or a *_bound call; the reviewed baseline only shrinks.',
      status: 'deferred',
      meta: { report_row: '1-static' },
    },
    {
      id: 'act-now.memory-pager-plan',
      kind: 'act-now',
      path: 'crates/store/src/ledger.rs',
      layers: ['contracts', 'state-model-crash'],
      lanes: ['pull', 'trusted', 'nightly'],
      test_ids: ['memory_pager_uses_a_half_open_range_not_like', 'memory_pager_plan_names_an_index_at_200k_rows'],
      evidence_kind: 'measurement',
      oracle:
        'EXPLAIN QUERY PLAN names the (session_id, kind, key) index for the V2 page and count; MEMORY_V2:x and memoryXv2:y do not match; 200k churn rows stay O(page).',
      status: 'deferred',
      meta: { report_row: 5 },
    },
    {
      id: 'act-now.scale-probe-schedule',
      kind: 'act-now',
      path: 'scripts/edition8/scale-probe.mjs',
      layers: ['state-model-crash', 'contracts'],
      lanes: ['nightly'],
      test_ids: ['programme.nightly.scale-probe'],
      evidence_kind: 'measurement',
      oracle:
        'The nightly lane runs the real 1,000-turn/100k-message/2k-session probe, stores curves and fails a quadratic shape even when absolute times look fine.',
      status: 'deferred',
      meta: { report_row: 'act-now-4' },
    },
    {
      id: 'act-now.event-ordering',
      kind: 'act-now',
      path: 'tests/programme/probes/ordering.mjs',
      layers: ['state-model-crash', 'contracts'],
      lanes: ['pull', 'trusted', 'nightly'],
      test_ids: ['event-stream.cursor-overflow', 'event-stream.duplicate-streams'],
      evidence_kind: 'state-model',
      oracle:
        'Overflow/negative/malformed cursors and duplicate concurrent streams are typed and bounded; no 5xx, no panic, no event duplication across one session.',
      status: 'deferred',
      meta: { report_row: 'widen-ordering' },
    },
    {
      id: 'act-now.hostile-generators',
      kind: 'act-now',
      path: 'tests/programme/generators/hostile.mjs',
      layers: ['unit-property', 'contracts'],
      lanes: ['pull', 'trusted'],
      test_ids: ['hostile.generator-selftest', 'hostile.route-cases'],
      evidence_kind: 'property',
      oracle:
        'Seed-stable oversized/duplicate-key/NUL/traversal/type-confusion inputs are generated within bounds and every route rejects them typed without a 5xx.',
      status: 'deferred',
      meta: { report_row: 'widen-hostile' },
    },
    {
      id: 'act-now.race-schedules',
      kind: 'act-now',
      path: 'tests/programme/generators/schedules.mjs',
      layers: ['state-model-crash', 'contracts'],
      lanes: ['pull', 'trusted', 'nightly'],
      test_ids: ['schedule.generator-selftest'],
      evidence_kind: 'state-model',
      oracle:
        'One settlement schedule exists per await point; duplicate/out-of-order/truncated event sequences and truncation/bitflip/length-lie corruptions are enumerable and replayable.',
      status: 'deferred',
      meta: { report_row: 'widen-race' },
    },
    {
      id: 'act-now.core-dump-history',
      kind: 'act-now',
      path: 'scripts/programme/check-core-dumps.mjs',
      layers: ['contracts'],
      lanes: ['pull', 'trusted'],
      test_ids: ['core-dump.scan-tree-and-history'],
      evidence_kind: 'static-scan',
      oracle:
        'No ELF core, minidump or Mach-O core above the allowlisted size is reachable from HEAD or any ref; a removed-but-untracked dump is reported too.',
      status: 'deferred',
      meta: { report_row: 19 },
    },
  ];
}

/**
 * The complete surface set, de-duplicated by id (a route discovered by the
 * server scanner is one surface, even if a second class also names it).
 */
export function discoverSurfaces(root = ROOT) {
  const rows = [
    ...baseRows(root),
    ...edition8Rows(root),
    ...invariantRows(root),
    ...configRows(root),
    ...adapterRows(root),
    ...actNowRows(),
  ];
  const byId = new Map();
  for (const row of rows) {
    if (byId.has(row.id)) {
      const existing = byId.get(row.id);
      existing.layers = [...new Set([...existing.layers, ...row.layers])].sort();
      existing.lanes = [...new Set([...existing.lanes, ...row.lanes])].sort();
      existing.test_ids = [...new Set([...existing.test_ids, ...row.test_ids])].sort();
      continue;
    }
    byId.set(row.id, {
      ...row,
      adversarial: [...(row.adversarial ?? ADVERSARIAL_BY_KIND[row.kind] ?? ['hostile-input'])],
      layers: [...new Set(row.layers)].sort(),
      lanes: [...new Set(row.lanes)].sort(),
      test_ids: [...new Set(row.test_ids)].sort(),
    });
  }
  return [...byId.values()].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

export function sourceDigests(root = ROOT) {
  const files = [
    'scripts/discover-capabilities.mjs',
    'tests/invariants.toml',
    'docs/specs/edition8/controls.json',
    'docs/specs/edition8/screens.md',
    'docs/specs/edition8/flows.md',
    'docs/specs/edition8/acceptance.md',
  ].filter((file) => existsSync(join(root, file)));
  return files;
}

export { REV8_TEST, CREDENTIAL_ROUTES };
