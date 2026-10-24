#!/usr/bin/env node
// Edition 8 gate runner.
//
// Runs every registry-driven gate in dependency order, converts each host's
// raw machine evidence into one `faktor-edition8-gate/v1` document, starts the
// real daemon for the production-wiring acceptance probes, and writes the
// certificate summary consumed by CI.
//
// Usage:
//   node scripts/edition8/run.mjs                     # all host/visual/perf gates
//   node scripts/edition8/run.mjs --dogfood           # + isolated real-daemon dogfood
//   node scripts/edition8/run.mjs --no-daemon         # skip daemon acceptance
//   node scripts/edition8/run.mjs --skip-jetbrains    # VS Code gates only
//   node scripts/edition8/run.mjs --no-verify         # write evidence, skip verify
//   node scripts/edition8/run.mjs --summary           # only re-write the summary
//   node scripts/edition8/run.mjs --allow-dirty       # local WIP checkout
//
// The runner is deterministic: no network beyond loopback, no time-based
// soak, no invented performance certification. Every gate is independently
// executable; this file only orders them and never hides a missing input.

import { execFileSync, spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  EXPECTED,
  buildRegistry,
  summarizeRegistry,
  validateRegistry,
} from '../../tests/edition8/registry.mjs';
import {
  GATES,
  OUT_DIR,
  writeGate,
  writeSummary,
  verifyGates,
} from '../../tests/edition8/evidence.mjs';
import { validateFixtures, validateHostWorkflows, workflowFixtures } from '../../tests/edition8/workflows.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const argv = process.argv.slice(2);
const flag = (name) => argv.includes(`--${name}`);
const arg = (name, fallback = null) => {
  const index = argv.indexOf(`--${name}`);
  return index >= 0 && index + 1 < argv.length ? argv[index + 1] : fallback;
};

const NO_VERIFY = flag('no-verify');
const SUMMARY_ONLY = flag('summary');
const SKIP_JETBRAINS = flag('skip-jetbrains');
const RUN_DOGFOOD = flag('dogfood') || flag('dogfood-evidence');
const DOGFOOD_EVIDENCE_ONLY = flag('dogfood-evidence');
const NO_DAEMON = flag('no-daemon');
const ALLOW_DIRTY = flag('allow-dirty');
const ALLOW_UNREVIEWED_PLATFORM = flag('allow-unreviewed-platform');
const BOOTSTRAP_BASELINES = flag('bootstrap-baselines');

function run(script, args = [], options = {}) {
  console.log(`[edition8] ${script}${args.length > 0 ? ` ${args.join(' ')}` : ''}`);
  execFileSync(process.execPath, [resolve(ROOT, script), ...args], {
    cwd: ROOT,
    stdio: 'inherit',
    env: { ...process.env, ...(options.env ?? {}) },
  });
}

function runShell(script, args = []) {
  console.log(`[edition8] ${script}${args.length > 0 ? ` ${args.join(' ')}` : ''}`);
  execFileSync('bash', [resolve(ROOT, script), ...args], {
    cwd: ROOT,
    stdio: 'inherit',
    env: { ...process.env },
  });
}

function raw(name) {
  const path = resolve(OUT_DIR, name);
  if (!existsSync(path)) throw new Error(`missing raw evidence ${name}; run its harness first`);
  return JSON.parse(readFileSync(path, 'utf8'));
}

function rawOrNull(name) {
  const path = resolve(OUT_DIR, name);
  if (!existsSync(path)) return null;
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

function requireCounts(document, expected, label) {
  for (const [key, value] of Object.entries(expected)) {
    if (document.counts?.[key] !== value) {
      throw new Error(`${label}: counts.${key} is ${String(document.counts?.[key])} (expected ${value})`);
    }
  }
}

function sha256(text) {
  return `sha256:${createHash('sha256').update(text).digest('hex')}`;
}

function cliBinary() {
  const candidates = [
    process.env.FAKTOR_CLI_BIN,
    resolve(ROOT, 'target/release/faktor-cli'),
    resolve(ROOT, 'target/debug/faktor-cli'),
  ].filter((candidate) => typeof candidate === 'string' && candidate.length > 0);
  return candidates.find((candidate) => existsSync(candidate)) ?? null;
}

async function withDaemon(fn) {
  const binary = cliBinary();
  if (binary === null) {
    throw new Error('no faktor-cli binary; build one or pass FAKTOR_CLI_BIN (or use --no-daemon)');
  }
  const tmp = mkdtempSync(join(tmpdir(), 'faktor-edition8-probe-'));
  const data = join(tmp, 'data');
  const config = join(tmp, 'faktor-plus.json');
  const token = randomBytes(32).toString('hex');
  mkdirSync(data, { recursive: true });
  writeFileSync(
    config,
    `${JSON.stringify({
      config_version: 1,
      model: 'm',
      providers: [
        {
          kind: 'open_ai',
          id: 'p1',
          base_url: 'http://127.0.0.1:9/v1',
          api_key_env: 'EDITION8_PROBE_KEY',
          api: 'chat',
          allow_loopback: true,
          models: ['m'],
        },
      ],
    })}\n`,
  );
  const child = spawn(binary, ['serve', '--port', '0', '--data-dir', data, '--config', config], {
    cwd: ROOT,
    env: { ...process.env, FAKTOR_SERVER_PASSWORD: token, EDITION8_PROBE_KEY: 'loopback-only' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  const collect = (chunk) => {
    output += chunk.toString();
  };
  child.stdout.on('data', collect);
  child.stderr.on('data', collect);
  let base = null;
  const deadline = Date.now() + 20_000;
  try {
    while (Date.now() < deadline) {
      const match = /(http:\/\/127\.0\.0\.1:\d+)/.exec(output);
      if (match !== null) {
        base = match[1];
        break;
      }
      if (child.exitCode !== null) break;
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
    }
    if (base === null) throw new Error(`daemon did not print its startup line: ${output.slice(-800)}`);
    let ready = false;
    while (Date.now() < deadline) {
      try {
        const response = await fetch(`${base}/native/ready`, {
          headers: { authorization: `Bearer ${token}` },
        });
        if (response.ok) {
          ready = true;
          break;
        }
      } catch {
        // keep polling the loopback endpoint
      }
      await new Promise((resolvePromise) => setTimeout(resolvePromise, 50));
    }
    if (!ready) throw new Error(`daemon never became ready: ${output.slice(-800)}`);
    return await fn({ base, token, binary });
  } finally {
    child.kill('SIGKILL');
    await new Promise((resolvePromise) => {
      if (child.exitCode !== null) resolvePromise();
      else child.once('exit', resolvePromise);
    });
    rmSync(tmp, { recursive: true, force: true });
  }
}

async function daemonAcceptance() {
  if (NO_DAEMON) {
    console.log('[edition8] daemon acceptance skipped by --no-daemon (typed boundary, no pass)');
    return false;
  }
  return withDaemon(async ({ base, token }) => {
    run('scripts/edition8/daemon-acceptance.mjs', ['--base-url', base, '--token', token]);
    return true;
  });
}

function convert() {
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) throw new Error(problems[0]);
  const fixtureProblems = validateFixtures();
  if (fixtureProblems.length > 0) throw new Error(fixtureProblems[0]);

  writeGate('registry', {
    status: 'passed',
    counts: { ...registry.counts },
    results: registry.screens.map((screen) => ({
      id: screen.id,
      title: screen.title,
      controls: screen.controls.length,
      contract_page: screen.contract_page,
    })),
    detail: {
      inputs_digest: registry.inputs_digest,
      acceptance_ids: registry.acceptance.map((scenario) => scenario.id),
      workflow_ids: registry.workflows.map((workflow) => workflow.id),
      workflow_fixtures: workflowFixtures().length,
      counts: summarizeRegistry(registry).counts,
    },
  });

  run('scripts/edition8/ledger.mjs', ['--check', '--require-live']);
  const ledger = JSON.parse(readFileSync(resolve(ROOT, 'docs/specs/edition8/functional-evidence.json'), 'utf8'));
  writeGate('ledger', {
    status: 'passed',
    counts: { controls: ledger.controls.length, ...ledger.counts.by_status },
    results: ledger.controls.map((row) => ({
      id: row.id,
      status: row.status,
      note: row.note,
      hosts: Object.fromEntries(
        Object.entries(row.hosts).map(([host, evidence]) => [host, evidence.status]),
      ),
    })),
    artifacts: { ledger: 'docs/specs/edition8/functional-evidence.json', controls: 'docs/specs/edition8/controls.json' },
    detail: { hosts: ledger.counts.by_host, backlog: 'docs/specs/edition8/backlog.md' },
  });

  const vscodeControls = raw('control-exercise-vscode.raw.json');
  requireCounts(
    vscodeControls,
    { controls: EXPECTED.controls, exercised: EXPECTED.controls, failed: 0, missing_handlers: 0, accessible_named: EXPECTED.controls },
    'vscode control exercise',
  );
  writeGate('control-exercise-vscode', {
    status: 'passed',
    counts: vscodeControls.counts,
    results: vscodeControls.results,
    artifacts: { raw: 'control-exercise-vscode.raw.json', screens: vscodeControls.screens ?? null },
  });

  const vscodeAcceptance = raw('acceptance-vscode.raw.json');
  requireCounts(
    vscodeAcceptance,
    { scenarios: EXPECTED.acceptance, passed: EXPECTED.acceptance, failed: 0 },
    'vscode acceptance',
  );
  for (const row of vscodeAcceptance.results ?? []) {
    for (const field of ['preconditions', 'steps', 'visible', 'field_checks', 'forbidden', 'source', 'receipt']) {
      if (row[field] === undefined || row[field] === null) throw new Error(`vscode acceptance ${row.id} lacks ${field}`);
    }
  }
  writeGate('acceptance-vscode', {
    status: 'passed',
    counts: vscodeAcceptance.counts,
    results: vscodeAcceptance.results,
    artifacts: { raw: 'acceptance-vscode.raw.json' },
  });

  const vscodeWorkflows = raw('workflows-vscode.raw.json');
  requireCounts(
    vscodeWorkflows,
    { workflows: EXPECTED.workflows, passed: EXPECTED.workflows, cells: EXPECTED.workflows * EXPECTED.cells_per_host, failed: 0 },
    'vscode workflows',
  );
  const vscodeWorkflowProblems = validateHostWorkflows(vscodeWorkflows, 'vscode');
  if (vscodeWorkflowProblems.length > 0) throw new Error(vscodeWorkflowProblems[0]);
  writeGate('workflows-vscode', {
    status: 'passed',
    counts: vscodeWorkflows.counts,
    results: vscodeWorkflows.results,
    artifacts: { raw: 'workflows-vscode.raw.json' },
  });

  if (!SKIP_JETBRAINS) {
    const jetbrainsControls = raw('control-exercise-jetbrains.raw.json');
    requireCounts(
      jetbrainsControls,
      { controls: EXPECTED.controls, exercised: EXPECTED.controls, failed: 0, missing_handlers: 0, accessible_named: EXPECTED.controls },
      'jetbrains control exercise',
    );
    writeGate('control-exercise-jetbrains', {
      status: 'passed',
      counts: jetbrainsControls.counts,
      results: jetbrainsControls.results,
      artifacts: { raw: 'control-exercise-jetbrains.raw.json', source_report: 'jetbrains-control-exercise.json' },
    });

    const jetbrainsAcceptance = raw('acceptance-jetbrains.raw.json');
    requireCounts(
      jetbrainsAcceptance,
      { scenarios: EXPECTED.acceptance, passed: EXPECTED.acceptance, failed: 0 },
      'jetbrains acceptance',
    );
    for (const row of jetbrainsAcceptance.results ?? []) {
      for (const field of ['preconditions', 'steps', 'visible', 'field_checks', 'forbidden', 'source', 'receipt']) {
        if (row[field] === undefined || row[field] === null) throw new Error(`jetbrains acceptance ${row.id} lacks ${field}`);
      }
    }
    writeGate('acceptance-jetbrains', {
      status: 'passed',
      counts: jetbrainsAcceptance.counts,
      results: jetbrainsAcceptance.results,
      artifacts: { raw: 'acceptance-jetbrains.raw.json', overlays: 'acceptance-jetbrains.overlays.json' },
    });

    const jetbrainsWorkflows = raw('workflows-jetbrains.raw.json');
    requireCounts(
      jetbrainsWorkflows,
      { workflows: EXPECTED.workflows, passed: EXPECTED.workflows, cells: EXPECTED.workflows * EXPECTED.cells_per_host, failed: 0 },
      'jetbrains workflows',
    );
    const jetbrainsWorkflowProblems = validateHostWorkflows(jetbrainsWorkflows, 'jetbrains');
    if (jetbrainsWorkflowProblems.length > 0) throw new Error(jetbrainsWorkflowProblems[0]);
    writeGate('workflows-jetbrains', {
      status: 'passed',
      counts: jetbrainsWorkflows.counts,
      results: jetbrainsWorkflows.results,
      artifacts: { raw: 'workflows-jetbrains.raw.json', source_report: 'workflows-jetbrains-host.json' },
    });
  }

  const visualContracts = raw('visual-contracts.raw.json');
  requireCounts(
    visualContracts,
    { screens: EXPECTED.screens, hosts: 2, contracts: EXPECTED.host_contracts, failed: 0, fixture_classes: EXPECTED.screens * 8 },
    'visual contracts',
  );
  writeGate('visual-contracts', {
    status: 'passed',
    counts: visualContracts.counts,
    results: visualContracts.results ?? [],
    artifacts: { raw: 'visual-contracts.raw.json' },
  });

  const platforms = raw('visual-platforms.raw.json');
  requireCounts(platforms, { screens: EXPECTED.screens, hosts: 2, failed: 0, records: EXPECTED.platform_records }, 'visual platforms');
  writeGate('visual-platforms', {
    status: 'passed',
    counts: platforms.counts,
    results: platforms.detail?.records ?? [],
    detail: platforms.detail,
    not_certified: platforms.not_certified ?? [],
    artifacts: {
      raw: 'visual-platforms.raw.json',
      vscode: 'visual-platforms-vscode.raw.json',
      jetbrains: 'visual-platforms-jetbrains.raw.json',
    },
  });

  const optical = raw('optical.raw.json');
  requireCounts(optical, { failed: 0 }, 'optical');
  writeGate('optical', {
    status: 'passed',
    counts: optical.counts,
    results: optical.results ?? [],
    detail: optical.detail,
    artifacts: { raw: 'optical.raw.json', vscode: 'optical-vscode.raw.json', jetbrains: 'optical-jetbrains.raw.json' },
  });

  const contrast = raw('contrast.raw.json');
  requireCounts(contrast, { failed: 0, pairs: 80, faktor_drawn_failures: 0 }, 'contrast');
  writeGate('contrast', {
    status: 'passed',
    counts: contrast.counts,
    results: contrast.findings ? [contrast.findings] : [],
    detail: { findings: contrast.findings ?? null, method: contrast.method ?? null },
    artifacts: { raw: 'contrast.raw.json', vscode: 'contrast-vscode.raw.json', jetbrains: 'contrast-jetbrains.raw.json' },
  });

  const copy = raw('copy-retention.raw.json');
  requireCounts(copy, { failed: 0, unaccounted: 0 }, 'copy retention');
  writeGate('copy-retention', {
    status: 'passed',
    counts: copy.counts,
    results: copy.results ?? [],
    detail: { printed: copy.printed ?? null },
    artifacts: { raw: 'copy-retention.raw.json', ledger: 'docs/specs/edition8/copy-retention.json' },
  });

  const assistive = raw('assistive.raw.json');
  requireCounts(assistive, { failed: 0, controls: EXPECTED.controls, accessible_named: EXPECTED.controls }, 'assistive');
  writeGate('assistive', {
    status: 'passed',
    counts: assistive.counts,
    detail: assistive.detail,
    artifacts: {
      raw: 'assistive.raw.json',
      vscode: 'control-exercise-vscode.raw.json',
      jetbrains: 'assistive-jetbrains.raw.json',
    },
  });

  const perf = raw('perf.raw.json');
  requireCounts(perf, { workloads: 11, failed: 0, baseline_present: 1 }, 'perf');
  writeGate('perf', {
    status: 'passed',
    counts: perf.counts,
    results: perf.results,
    detail: {
      budget: perf.budget,
      baseline: perf.baseline,
      baseline_source: perf.baseline_source,
      baseline_committed: perf.baseline_committed === true,
      platform: perf.platform,
      static_scan: perf.static_scan,
    },
    artifacts: { raw: 'perf.raw.json', baseline: 'tests/edition8/perf-baseline.json' },
  });

  const scale = rawOrNull('scale.raw.json');
  if (scale !== null) {
    requireCounts(
      scale,
      {
        failed: 0,
        probes: 10,
        turns: 1000,
        messages: 100000,
        memory_facts: 5000,
        history_sessions: 2000,
        children: 500,
        claims: 10000,
        concurrent_sessions: 16,
      },
      'scale probe',
    );
    writeGate('scale', {
      status: 'passed',
      counts: scale.counts,
      results: scale.probes,
      detail: { metrics: scale.metrics, platform: scale.platform },
      artifacts: { raw: 'scale.raw.json' },
    });
  }

  const daemon = rawOrNull('acceptance-daemon.raw.json');
  if (daemon !== null) {
    requireCounts(daemon, { scenarios: EXPECTED.acceptance, passed: EXPECTED.acceptance, failed: 0 }, 'daemon acceptance');
    writeGate('acceptance-daemon', {
      status: 'passed',
      counts: daemon.counts,
      results: daemon.results,
      artifacts: { raw: 'acceptance-daemon.raw.json' },
    });
  }

  const dogfood = rawOrNull('dogfood.raw.json');
  if (dogfood !== null) {
    writeGate('dogfood', {
      status: dogfood.counts?.failed === 0 ? 'passed' : 'failed',
      counts: dogfood.counts ?? {},
      results: dogfood.results ?? [],
      detail: {
        isolated: dogfood.isolated === true,
        network: dogfood.network,
        worktree: dogfood.worktree,
        base_url: dogfood.base_url,
      },
      artifacts: {
        raw: 'dogfood.raw.json',
        vscode: 'dogfood-vscode.raw.json',
        jetbrains: 'dogfood-jetbrains.raw.json',
      },
    });
  }

  const certificate = {
    schema: 'faktor-edition8-certificate-gate/v1',
    generated_at: new Date().toISOString(),
    counts: {
      screens: EXPECTED.screens,
      host_contracts: EXPECTED.host_contracts,
      controls: EXPECTED.controls,
      acceptance: EXPECTED.acceptance,
      workflows: EXPECTED.workflows,
      workflow_cells: EXPECTED.cells_total,
      platform_records: EXPECTED.platform_records,
      gates: Object.keys(GATES).length,
    },
    digest: sha256(JSON.stringify(registry.inputs_digest)),
  };
  writeGate('certificate', {
    status: 'passed',
    counts: certificate.counts,
    results: [{ id: 'edition8-certificate', passed: true, detail: 'evidence documents written for every gate' }],
    detail: certificate,
  });
}

async function main() {
  const registry = buildRegistry();
  const problems = validateRegistry(registry);
  if (problems.length > 0) {
    for (const problem of problems) console.error(`edition8 registry: ${problem}`);
    process.exit(1);
  }
  console.log(
    `[edition8] registry ${registry.counts.controls} controls / ${registry.counts.screens} screens / ` +
      `${registry.counts.host_contracts} host contracts / ${registry.counts.acceptance} acceptance / ${registry.counts.workflows} workflows`,
  );

  let bootstrappedVisual = false;
  let perfBaselineFile = null;
  if (!SUMMARY_ONLY) {
    if (BOOTSTRAP_BASELINES) {
      const platformName = process.platform === 'darwin' ? 'macos' : process.platform === 'win32' ? 'windows' : 'linux';
      const visualBaseline = resolve(ROOT, `apps/vscode/scripts/baselines/edition8/${platformName}.json`);
      if (!existsSync(visualBaseline)) {
        console.log(`[edition8] no reviewed ${platformName} visual baseline; writing an unreviewed matrix baseline for this runner`);
        run('apps/vscode/scripts/edition8-visual.mjs', ['--write']);
        bootstrappedVisual = true;
      }
      const perfBaseline = JSON.parse(readFileSync(resolve(ROOT, 'tests/edition8/perf-baseline.json'), 'utf8'));
      if (perfBaseline.platforms?.[platformName] === undefined) {
        console.log(`[edition8] no committed ${platformName} performance baseline; measuring a runner-local baseline (not a regression certificate)`);
        perfBaselineFile = resolve(OUT_DIR, `perf-baseline-${platformName}.json`);
        run('tests/edition8/perf.mjs', ['--write-baseline', '--baseline-file', perfBaselineFile]);
      }
    }
    run('tests/edition8/registry.mjs');
    run('tests/edition8/visual-contracts.mjs');
    run('apps/vscode/scripts/edition8-control-exercise.mjs');
    run('apps/vscode/scripts/edition8-acceptance.mjs');
    run('apps/vscode/scripts/transcript-deny-check.mjs');
    run('apps/vscode/scripts/edition8-workflows.mjs');
    if (!bootstrappedVisual) run('apps/vscode/scripts/edition8-visual.mjs', ['--check']);
    run('apps/vscode/scripts/edition8-optical.mjs');
    run('apps/vscode/scripts/edition8-contrast.mjs');

    if (!SKIP_JETBRAINS) {
      const jetbrainsInputs = [
        'control-exercise-jetbrains.raw.json',
        'acceptance-jetbrains.raw.json',
        'workflows-jetbrains.raw.json',
        'visual-platforms-jetbrains.raw.json',
        'optical-jetbrains.raw.json',
        'contrast-jetbrains.raw.json',
        'assistive-jetbrains.raw.json',
      ];
      // Always re-derive the JetBrains raws from the current source reports.
      // Previously this ran only when a derived file was missing, so a fresh
      // host report (04:46) was silently paired with raws converted from an
      // older run (04:09) and the summary looked current. Conversions are
      // local and cheap; a missing source fails with its own diagnostic.
      const missing = jetbrainsInputs.filter((name) => !existsSync(resolve(OUT_DIR, name)));
      console.log(
        `[edition8] converting the JetBrains raw reports from the jetbrains-smoke lane` +
          (missing.length > 0 ? ` (missing ${missing.join(', ')})` : ' (refresh)'),
      );
      run('scripts/edition8/jetbrains-control-exercise.mjs');
      run('scripts/edition8/jetbrains-acceptance.mjs');
      run('scripts/edition8/jetbrains-workflows.mjs');
      const jbPlatform = process.env.E8_JETBRAINS_PLATFORM;
      const jbArgs = jbPlatform
        ? [
            '--platform',
            jbPlatform,
            '--os',
            process.env.E8_JETBRAINS_OS ?? `${jbPlatform} render`,
            '--build',
            process.env.E8_JETBRAINS_BUILD ?? 'unknown',
            '--font',
            process.env.E8_JETBRAINS_FONT ?? `java-host-ui-font-${jbPlatform}`,
          ]
        : [];
      run('scripts/edition8/jetbrains-visual.mjs', jbArgs);
      run('tests/edition8/platforms.mjs');
      run('tests/edition8/optical.mjs');
      run('tests/edition8/contrast.mjs');
      run('tests/edition8/assistive.mjs');
    } else {
      run('tests/edition8/platforms.mjs');
      run('tests/edition8/optical.mjs');
      run('tests/edition8/contrast.mjs');
      run('tests/edition8/assistive.mjs');
    }

    run('tests/edition8/copy-retention.mjs');
    run('tests/edition8/perf.mjs', ['--check', ...(perfBaselineFile === null ? [] : ['--baseline-file', perfBaselineFile])]);

    if (RUN_DOGFOOD && !DOGFOOD_EVIDENCE_ONLY) {
      runShell('scripts/edition8/dogfood.sh');
    }
    if (DOGFOOD_EVIDENCE_ONLY && !existsSync(resolve(OUT_DIR, 'dogfood.raw.json'))) {
      throw new Error('--dogfood-evidence requires target/certification/edition8/dogfood.raw.json');
    }
    await daemonAcceptance();
    convert();
  }

  mkdirSync(OUT_DIR, { recursive: true });
  const summary = writeSummary({ requireDogfood: RUN_DOGFOOD, allowUnreviewedPlatform: ALLOW_UNREVIEWED_PLATFORM });
  console.log(`[edition8] summary written: ${resolve(ROOT, 'target/certification/edition8-summary.json')}`);
  console.log(JSON.stringify(summary.counts));

  if (!NO_VERIFY && SKIP_JETBRAINS) {
    console.log('[edition8] verification limited to VS Code by --skip-jetbrains; JetBrains gates remain a typed missing boundary');
  } else if (!NO_VERIFY) {
    const verifyProblems = verifyGates({
      allowDirty: ALLOW_DIRTY,
      requireDogfood: RUN_DOGFOOD,
      allowUnreviewedPlatform: ALLOW_UNREVIEWED_PLATFORM,
    });
    if (verifyProblems.length > 0) {
      for (const problem of verifyProblems) console.error(`edition8 evidence: ${problem}`);
      process.exit(1);
    }
    console.log('[edition8] all gates present, current and counted');
  } else {
    console.log('[edition8] verification skipped by --no-verify');
  }
}

const keepAlive = setInterval(() => {}, 1 << 30);
main()
  .then(() => {
    clearInterval(keepAlive);
  })
  .catch((error) => {
    clearInterval(keepAlive);
    console.error(`edition8: ${error && error.stack ? error.stack : String(error)}`);
    process.exit(1);
  });
