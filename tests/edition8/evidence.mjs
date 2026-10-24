#!/usr/bin/env node
// Machine-readable evidence for every Edition 8 gate. Each gate writes one
// `faktor-edition8-gate/v1` document under target/certification/edition8/;
// `node tests/edition8/evidence.mjs verify` re-reads every one and refuses a
// missing, stale, under-counted or cross-platform-inherited document.
//
// Counts are never inferred from prose: callers pass the actual exercised
// numbers and `verify` re-derives the required totals from the registry.

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  EXPECTED,
  buildRegistry,
  summarizeRegistry,
  validateRegistry,
} from './registry.mjs';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
export const OUT_DIR = resolve(ROOT, 'target/certification/edition8');
export const SUMMARY_PATH = resolve(ROOT, 'target/certification/edition8-summary.json');
export const LANE_PATH = resolve(ROOT, 'target/certification/lanes/edition8.json');
export const GATE_SCHEMA = 'faktor-edition8-gate/v1';
export const SUMMARY_SCHEMA = 'faktor-edition8-certificate/v1';
export const RAW_SCHEMA = 'faktor-edition8-raw/v1';

export const GATES = Object.freeze({
  registry: { kind: 'registry', host: 'both' },
  ledger: { kind: 'ledger', host: 'both' },
  'control-exercise-vscode': { kind: 'control-exercise', host: 'vscode' },
  'control-exercise-jetbrains': { kind: 'control-exercise', host: 'jetbrains' },
  'acceptance-vscode': { kind: 'acceptance', host: 'vscode' },
  'acceptance-jetbrains': { kind: 'acceptance', host: 'jetbrains' },
  'acceptance-daemon': { kind: 'acceptance', host: 'daemon' },
  'workflows-vscode': { kind: 'workflows', host: 'vscode' },
  'workflows-jetbrains': { kind: 'workflows', host: 'jetbrains' },
  'visual-contracts': { kind: 'visual-contracts', host: 'both' },
  'visual-platforms': { kind: 'visual-platforms', host: 'both' },
  optical: { kind: 'optical', host: 'both' },
  contrast: { kind: 'contrast', host: 'both' },
  'copy-retention': { kind: 'copy-retention', host: 'both' },
  assistive: { kind: 'assistive', host: 'both' },
  perf: { kind: 'perf', host: 'both' },
  scale: { kind: 'scale', host: 'both' },
  dogfood: { kind: 'dogfood', host: 'both' },
  certificate: { kind: 'certificate', host: 'both' },
});

export const PLATFORM_KEYS = Object.freeze(['linux', 'macos', 'windows']);

function git(args) {
  try {
    return execFileSync('git', ['-C', ROOT, ...args], { encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
}

export function headIdentity() {
  return {
    commit: git(['rev-parse', 'HEAD']),
    tree: git(['rev-parse', 'HEAD^{tree}']),
    dirty: (git(['status', '--porcelain', '--untracked-files=no']) ?? '').length > 0,
  };
}

export function sha256Bytes(bytes) {
  return `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
}

export function artifactDigest(path) {
  if (!existsSync(path)) return null;
  return sha256Bytes(readFileSync(path));
}

export function nowIso() {
  return new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
}

export function writeRaw(name, payload) {
  mkdirSync(OUT_DIR, { recursive: true });
  const path = resolve(OUT_DIR, name);
  writeFileSync(path, `${JSON.stringify(payload, null, 2)}\n`);
  return { path, sha256: artifactDigest(path) };
}

export function readRaw(name) {
  const path = resolve(OUT_DIR, name);
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

export function writeGate(name, payload) {
  const spec = GATES[name];
  if (!spec) throw new Error(`unregistered edition8 gate ${name}`);
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) throw new Error(problems[0]);
  const identity = headIdentity();
  const artifacts = {};
  for (const [key, value] of Object.entries(payload.artifacts ?? {})) {
    artifacts[key] = typeof value === 'string' && existsSync(resolve(ROOT, value))
      ? artifactDigest(resolve(ROOT, value))
      : value;
  }
  const document = {
    schema: GATE_SCHEMA,
    gate: name,
    kind: spec.kind,
    host: spec.host,
    registry: summarizeRegistry(registry),
    commit: identity.commit,
    tree: identity.tree,
    dirty: identity.dirty,
    generated_at: nowIso(),
    status: payload.status ?? 'passed',
    counts: payload.counts ?? {},
    results: payload.results ?? [],
    artifacts,
    detail: payload.detail ?? null,
    not_certified: payload.not_certified ?? [],
  };
  mkdirSync(OUT_DIR, { recursive: true });
  writeFileSync(resolve(OUT_DIR, `${name}.json`), `${JSON.stringify(document, null, 2)}\n`);
  return document;
}

export function readGate(name) {
  const path = resolve(OUT_DIR, `${name}.json`);
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

export function writeSummary({ requireDogfood = false, allowUnreviewedPlatform = false } = {}) {
  const registry = buildRegistry();
  const identity = headIdentity();
  const registrySummary = summarizeRegistry(registry);
  const gates = {};
  for (const name of Object.keys(GATES)) {
    const doc = readGate(name);
    gates[name] = doc === null
      ? { status: 'missing' }
      : {
          status: doc.status,
          counts: doc.counts,
          generated_at: doc.generated_at,
          artifacts: doc.artifacts,
        };
  }
  const required = Object.keys(GATES).filter((name) => requireDogfood || name !== 'dogfood');
  const missing = required.filter((name) => gates[name]?.status !== 'passed');
  const platforms = readGate('visual-platforms');
  const currentPlatform = platforms?.detail?.current_platform ?? null;
  const currentRecords = (platforms?.detail?.records ?? []).filter((record) => record.platform === currentPlatform);
  const currentCertified = currentRecords.length > 0 && currentRecords.every((record) => record.status === 'certified');
  const perfCommitted = readGate('perf')?.detail?.baseline_committed !== false;
  const status = missing.length > 0
    ? 'incomplete'
    : (currentCertified && perfCommitted) || !allowUnreviewedPlatform
      ? 'certified_local'
      : 'host_gates_passed_visual_boundary';
  const summary = {
    schema: SUMMARY_SCHEMA,
    edition: 8,
    revision: registry.revision,
    commit: identity.commit,
    tree: identity.tree,
    dirty: identity.dirty,
    generated_at: nowIso(),
    registry: registrySummary,
    counts: {
      ...registry.counts,
      gates: required.length,
      passed_gates: required.filter((name) => gates[name]?.status === 'passed').length,
      optional_gates_present: Object.keys(GATES).filter((name) => !required.includes(name) && gates[name]?.status === 'passed').length,
      ...(readGate('visual-platforms')?.counts ?? {}),
      // Explicit certificate axes: platforms certified from their OWN render
      // (macOS + Linux, both hosts), platform boundaries left typed, and the
      // AT contracts that remain typed because no real screen-reader session
      // ran. These are counts, never claims.
      platforms_certified: readGate('visual-platforms')?.counts?.certified ?? 0,
      platforms_typed: readGate('visual-platforms')?.counts?.not_certified ?? 0,
      at_certified: readGate('assistive')?.detail?.at_certified ?? 0,
      at_typed_boundaries: readGate('assistive')?.detail?.at_not_certified ?? 0,
    },
    status,
    missing_gates: missing,
    gates,
    not_certified: readGate('visual-platforms')?.not_certified ?? [],
  };
  mkdirSync(dirname(SUMMARY_PATH), { recursive: true });
  writeFileSync(SUMMARY_PATH, `${JSON.stringify(summary, null, 2)}\n`);
  return summary;
}

function requireCount(doc, key, expected, label, problems) {
  if (doc.counts?.[key] !== expected) {
    problems.push(`${doc.gate}: ${label} ${String(doc.counts?.[key])} != ${expected}`);
  }
}

function rowsCover(rows, ids, label, problems) {
  if (!Array.isArray(rows) || rows.length !== ids.length) {
    problems.push(`${label}: expected ${ids.length} result rows, got ${Array.isArray(rows) ? rows.length : 0}`);
    return;
  }
  const actual = new Set(rows.map((row) => row.id));
  for (const id of ids) {
    if (!actual.has(id)) problems.push(`${label}: missing ${id}`);
  }
  if (actual.size !== ids.length) problems.push(`${label}: duplicate or unknown result ids`);
}

export function verifyGates({ allowDirty = false, requireDogfood = false, allowUnreviewedPlatform = false } = {}) {
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) return problems;
  const identity = headIdentity();
  const docs = {};
  const required = Object.keys(GATES).filter((name) => requireDogfood || name !== 'dogfood');
  for (const name of required) {
    const doc = readGate(name);
    if (doc === null) {
      problems.push(`missing evidence: ${name}`);
      continue;
    }
    docs[name] = doc;
    if (doc.schema !== GATE_SCHEMA) problems.push(`${name}: wrong schema ${String(doc.schema)}`);
    if (doc.commit !== identity.commit) problems.push(`${name}: bound to another commit`);
    if (doc.tree !== identity.tree) problems.push(`${name}: bound to another tree`);
    if (doc.registry.inputs_digest !== summarizeRegistry(registry).inputs_digest) {
      problems.push(`${name}: registry inputs changed after evidence was written`);
    }
    if (doc.status !== 'passed') problems.push(`${name}: status ${doc.status}`);
    if (doc.dirty && !allowDirty) problems.push(`${name}: recorded a dirty checkout`);
  }

  const ledger = docs.ledger;
  if (ledger) {
    requireCount(ledger, 'controls', EXPECTED.controls, 'controls', problems);
    const byStatus = ledger.counts ?? {};
    if ((byStatus.exists ?? 0) + (byStatus.partial ?? 0) + (byStatus.missing ?? 0) !== EXPECTED.controls) {
      problems.push('ledger: exists + partial + missing must equal 200');
    }
    if ((byStatus.missing ?? 0) !== 0) {
      problems.push(`ledger: ${byStatus.missing} controls have no implementation evidence`);
    }
    if (!Array.isArray(ledger.results) || ledger.results.length !== EXPECTED.controls) {
      problems.push('ledger: per-control evidence rows are missing');
    }
  }

  const controlIds = registry.controls.map((control) => control.id);
  const acceptanceIds = registry.acceptance.map((scenario) => scenario.id);
  const workflowIds = registry.workflows.map((workflow) => workflow.id);

  for (const host of ['vscode', 'jetbrains']) {
    const controls = docs[`control-exercise-${host}`];
    if (controls) {
      requireCount(controls, 'controls', EXPECTED.controls, 'controls', problems);
      requireCount(controls, 'exercised', EXPECTED.controls, 'exercised', problems);
      requireCount(controls, 'failed', 0, 'failed', problems);
      requireCount(controls, 'missing_handlers', 0, 'missing handlers', problems);
      requireCount(controls, 'accessible_named', EXPECTED.controls, 'accessible named', problems);
      if (controls.counts?.effective + controls.counts?.unavailable !== EXPECTED.controls) {
        problems.push(`${host}: effective + unavailable must equal ${EXPECTED.controls}`);
      }
      rowsCover(controls.results, controlIds, `${host} controls`, problems);
      for (const row of controls.results ?? []) {
        if (!row.observable) problems.push(`${host}: ${row.id} has no observable effect`);
        if (!row.handler) problems.push(`${host}: ${row.id} has no handler`);
        if (!row.role) problems.push(`${host}: ${row.id} has no accessible role`);
        if (row.focusable !== true) problems.push(`${host}: ${row.id} is not keyboard focusable`);
      }
    }

    const acceptance = docs[`acceptance-${host}`];
    if (acceptance) {
      requireCount(acceptance, 'scenarios', EXPECTED.acceptance, 'scenarios', problems);
      requireCount(acceptance, 'passed', EXPECTED.acceptance, 'passed', problems);
      requireCount(acceptance, 'failed', 0, 'failed', problems);
      rowsCover(acceptance.results, acceptanceIds, `${host} acceptance`, problems);
      for (const row of acceptance.results ?? []) {
        for (const field of ['preconditions', 'steps', 'visible', 'field_checks', 'forbidden']) {
          const value = row[field];
          const empty = value === undefined || value === null || (Array.isArray(value) && value.length === 0) ||
            (typeof value === 'object' && !Array.isArray(value) && Object.keys(value).length === 0);
          if (empty) problems.push(`${host}: ${row.id} lacks ${field}`);
        }
        if (!row.source || !String(row.source).includes('p.343')) problems.push(`${host}: ${row.id} has no PDF p.343 source binding`);
        if (row.passed === true && (!row.receipt || row.receipt.host !== host)) {
          problems.push(`${host}: ${row.id} passed without a ${host} host receipt`);
        }
        if (row.passed !== true && !row.reason) problems.push(`${host}: ${row.id} failed without a typed reason`);
      }
    }

    const workflows = docs[`workflows-${host}`];
    if (workflows) {
      requireCount(workflows, 'workflows', EXPECTED.workflows, 'workflows', problems);
      requireCount(workflows, 'passed', EXPECTED.workflows, 'passed', problems);
      requireCount(workflows, 'cells', EXPECTED.workflows * EXPECTED.cells_per_host, 'cells', problems);
      requireCount(workflows, 'failed', 0, 'failed', problems);
      rowsCover(workflows.results, workflowIds, `${host} workflows`, problems);
      for (const row of workflows.results ?? []) {
        for (const cell of ['pointer', 'keyboard', 'screen_reader', 'compact_zoom', 'recovery']) {
          const value = row.cells?.[cell];
          if (!value || value.status !== 'passed') {
            problems.push(`${host}: ${row.id} ${cell} is not passed`);
          }
        }
      }
    }
  }

  const daemon = docs['acceptance-daemon'];
  if (daemon) {
    requireCount(daemon, 'scenarios', EXPECTED.acceptance, 'scenarios', problems);
    requireCount(daemon, 'passed', EXPECTED.acceptance, 'passed', problems);
    requireCount(daemon, 'production_routes', daemon.counts?.production_routes, 'production routes', problems);
    rowsCover(daemon.results, acceptanceIds, 'daemon acceptance', problems);
  }

  const visual = docs['visual-contracts'];
  if (visual) {
    requireCount(visual, 'screens', EXPECTED.screens, 'screens', problems);
    requireCount(visual, 'hosts', 2, 'hosts', problems);
    requireCount(visual, 'contracts', EXPECTED.host_contracts, 'contracts', problems);
    requireCount(visual, 'failed', 0, 'failed', problems);
    requireCount(visual, 'fixture_classes', EXPECTED.screens * 8, 'fixture classes', problems);
  }

  const platforms = docs['visual-platforms'];
  if (platforms) {
    requireCount(platforms, 'screens', EXPECTED.screens, 'screens', problems);
    requireCount(platforms, 'hosts', 2, 'hosts', problems);
    requireCount(platforms, 'failed', 0, 'failed', problems);
    const records = platforms.detail?.records ?? [];
    if (records.length !== EXPECTED.platform_records) {
      problems.push(`visual-platforms: ${records.length} records != ${EXPECTED.platform_records}`);
    }
    const current = process.platform === 'darwin' ? 'macos' : process.platform === 'linux' ? 'linux' : 'windows';
    for (const record of records) {
      if (record.status === 'certified' && !record.platform) {
        problems.push('visual-platforms: certified record without a platform');
      }
      if (record.status === 'not_certified' && !(record.reason && record.reason.length > 0)) {
        problems.push(`visual-platforms: ${record.host}/${record.platform} has no typed reason`);
      }
      if (!allowUnreviewedPlatform && record.host === 'vscode' && record.platform === current && record.status !== 'certified') {
        problems.push(`visual-platforms: current platform ${current} is not certified for vscode`);
      }
      if (!allowUnreviewedPlatform && record.host === 'jetbrains' && record.platform === current && record.status !== 'certified') {
        problems.push(`visual-platforms: current platform ${current} is not certified for jetbrains`);
      }
    }
    for (const record of records) {
      if (record.status === 'certified') {
        for (const field of ['build', 'os', 'font_fingerprint', 'sha256', 'cells']) {
          if (record[field] === undefined || record[field] === null) {
            problems.push(`visual-platforms: ${record.host}/${record.platform} lacks ${field}`);
          }
        }
      }
    }
  }

  const optical = docs.optical;
  if (optical) {
    requireCount(optical, 'failed', 0, 'failed', problems);
    const hosts = optical.detail?.measurements ?? {};
    for (const host of ['vscode', 'jetbrains']) {
      const themes = Object.keys(hosts[host] ?? {});
      if (themes.length === 0) problems.push(`optical: ${host} has no theme measurements`);
      for (const theme of themes) {
        const entry = hosts[host][theme];
        const count = entry.buttons ?? 0;
        if (count < 30) problems.push(`optical: ${host}/${theme} measured ${count} buttons < 30`);
        const median = Math.abs(entry.median ?? 99);
        const maxAbs = Math.abs(entry.max_abs ?? 99);
        if (maxAbs > 2.0) problems.push(`optical: ${host}/${theme} max offset ${maxAbs} > 2.0 px`);
        if (median > 0.5) problems.push(`optical: ${host}/${theme} median offset ${median} > 0.5 px`);
      }
    }
  }

  const contrast = docs.contrast;
  if (contrast) {
    requireCount(contrast, 'failed', 0, 'failed', problems);
    requireCount(contrast, 'pairs', 80, 'pairs', problems);
    requireCount(contrast, 'faktor_drawn_failures', 0, 'Faktor-drawn failures', problems);
  }

  const copy = docs['copy-retention'];
  if (copy) {
    requireCount(copy, 'failed', 0, 'failed', problems);
    requireCount(copy, 'unaccounted', 0, 'unaccounted segments', problems);
  }

  const assistive = docs.assistive;
  if (assistive) {
    requireCount(assistive, 'failed', 0, 'failed', problems);
    requireCount(assistive, 'controls', EXPECTED.controls, 'controls', problems);
    requireCount(assistive, 'accessible_named', EXPECTED.controls, 'accessible named', problems);
    const boundaries = assistive.detail?.at_boundaries ?? [];
    if (boundaries.length !== 3) problems.push(`assistive: ${boundaries.length} AT records != 3`);
    const atFlows = assistive.detail?.at_flows ?? [];
    for (const flow of ['W07', 'W14', 'W16', 'A39']) {
      for (const at of ['NVDA', 'VoiceOver', 'Orca']) {
        if (!atFlows.some((record) => record.flow === flow && record.at === at)) problems.push(`assistive: missing ${at} ${flow} record`);
      }
    }
    for (const record of atFlows) {
      if (record.status === 'not_certified' && !(record.reason && record.reason.length > 0)) {
        problems.push(`assistive: ${record.at} ${record.flow} boundary lacks a reason`);
      }
    }
    for (const boundary of boundaries) {
      if (!['certified', 'not_certified'].includes(boundary.status)) {
        problems.push(`assistive: ${boundary.at} has unknown status ${boundary.status}`);
      }
      if (boundary.status === 'not_certified' && !(boundary.reason && boundary.reason.length > 0)) {
        problems.push(`assistive: ${boundary.at} lacks a typed boundary reason`);
      }
    }
  }

  const perf = docs.perf;
  if (perf) {
    requireCount(perf, 'workloads', 11, 'workloads', problems);
    requireCount(perf, 'failed', 0, 'failed', problems);
    requireCount(perf, 'baseline_present', 1, 'baseline present', problems);
    const budget = perf.detail?.budget ?? {};
    if (!(budget.p95_ratio_max <= 1.25)) problems.push('perf: p95 budget is not the committed 1.25x bound');
    for (const sample of perf.results ?? []) {
      if ((sample.dropped ?? 0) !== 0 || (sample.duplicated ?? 0) !== 0 || (sample.reordered ?? 0) !== 0) {
        problems.push(`perf: ${sample.id} dropped/duplicated/reordered events`);
      }
      if ((sample.p95_ratio ?? 99) > 1.25) problems.push(`perf: ${sample.id} p95 ratio ${sample.p95_ratio} > 1.25`);
      if (sample.bounded !== true) problems.push(`perf: ${sample.id} is not bounded`);
    }
  }

  if (requireDogfood) {
    const dogfood = docs.dogfood;
    if (dogfood) {
      requireCount(dogfood, 'failed', 0, 'failed', problems);
      if (dogfood.detail?.isolated !== true) problems.push('dogfood: not isolated');
      if (dogfood.detail?.network !== 'loopback-only') problems.push('dogfood: network is not loopback-only');
      requireCount(dogfood, 'workflows', EXPECTED.workflows, 'workflows', problems);
      requireCount(dogfood, 'workflow_passed', EXPECTED.workflows, 'workflow passed', problems);
      for (const id of ['task-admission', 'provider-config', 'compaction-long-task', 'restore-fork', 'permissions', 'proof', 'finish-levels']) {
        if (!(dogfood.results ?? []).some((row) => row.id === id && row.passed === true)) {
          problems.push(`dogfood: missing passed flow ${id}`);
        }
      }
      for (const host of ['vscode', 'jetbrains']) {
        const count = dogfood.counts?.[`${host}_flows`];
        if (!(count >= EXPECTED.workflows)) problems.push(`dogfood: ${host} flows ${String(count)} < ${EXPECTED.workflows}`);
      }
    }
  }

  if (existsSync(SUMMARY_PATH)) {
    try {
      const summary = JSON.parse(readFileSync(SUMMARY_PATH, 'utf8'));
      if (summary.schema !== SUMMARY_SCHEMA) problems.push('edition8 summary: wrong schema');
      if (summary.commit !== identity.commit || summary.tree !== identity.tree) {
        problems.push('edition8 summary: bound to another commit/tree');
      }
      const allowedStatus = allowUnreviewedPlatform ? ['certified_local', 'host_gates_passed_visual_boundary'] : ['certified_local'];
      if (!allowedStatus.includes(summary.status)) problems.push(`edition8 summary: status ${summary.status}`);
      if ((summary.counts?.screens ?? 0) !== EXPECTED.screens) problems.push('edition8 summary: wrong screen count');
    } catch (error) {
      problems.push(`edition8 summary: unreadable (${error && error.message})`);
    }
  }
  return problems;
}

export function evidenceFiles() {
  if (!existsSync(OUT_DIR)) return [];
  return readdirSync(OUT_DIR).filter((name) => name.endsWith('.json')).sort();
}

function main() {
  const verify = process.argv.includes('verify');
  const allowDirty = process.argv.includes('--allow-dirty');
  const requireDogfood = process.argv.includes('--dogfood');
  const allowUnreviewedPlatform = process.argv.includes('--allow-unreviewed-platform');
  if (!verify) {
    console.log('usage: node tests/edition8/evidence.mjs verify [--allow-dirty] [--dogfood]');
    process.exit(1);
  }
  const summary = writeSummary({ requireDogfood, allowUnreviewedPlatform });
  const problems = verifyGates({ allowDirty, requireDogfood, allowUnreviewedPlatform });
  for (const problem of problems) console.error(`edition8 evidence: ${problem}`);
  console.log(
    `edition8 evidence: ${summary.counts.passed_gates}/${summary.counts.gates} gates; ` +
      `${summary.status}${problems.length === 0 ? '' : `; ${problems.length} problems`}`,
  );
  if (problems.length > 0) process.exit(1);
}

if (process.argv[1] !== undefined && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
