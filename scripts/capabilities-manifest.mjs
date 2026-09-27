#!/usr/bin/env node
// Machine-readable capability manifest (certification truth audit).
//
// Every status is DERIVED from repository files and scripts — never from
// prose. The generator probes the Faktor-owned VS Code panel sources, the
// JetBrains backend/frontend sources, the executable parity artifacts and
// the ACP subset, then writes:
//
//   target/certification/capabilities.json
//
// The same manifest carries the per-capability CERTIFICATION AXES
// (`axes.<capability>.{wired, adapter_certified, daemon_e2e_certified,
// platforms_certified, external_dependency}`), derived from production
// construction markers and the `tests/production-wiring` suite (whose tests
// drive the executable's own `build_daemon_core` with fake external seams),
// plus the Woodpecker lanes that actually run the workspace tests. Prose
// never moves a field: the README claims block is compared field-by-field
// against this manifest.
//
// Usage:
//   node scripts/capabilities-manifest.mjs                 generate + drift check
//   node scripts/capabilities-manifest.mjs --generate-only  skip the docs check
//
// Env:
//   CAPABILITIES_OUT_DIR  output directory (default target/certification)

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const OUT_DIR = process.env.CAPABILITIES_OUT_DIR || 'target/certification';
const MANIFEST_PATH = resolve(ROOT, OUT_DIR, 'capabilities.json');
const DOC_PATH = resolve(ROOT, 'docs/certification.md');
const GENERATE_ONLY = process.argv.includes('--generate-only');
const CHECK_CLAIMS = process.argv.includes('--check-claims');

const file = (rel) => existsSync(resolve(ROOT, rel));

// A DIRECTORY probe must actually test isDirectory(): `exists && !exists`
// is dead code and silently kept the vendored-JetBrains flip unreachable.
const dir = (rel) => {
  const path = resolve(ROOT, rel);
  if (!existsSync(path)) {
    return false;
  }
  try {
    return statSync(path).isDirectory();
  } catch {
    return false;
  }
};

function readText(rel) {
  return readFileSync(resolve(ROOT, rel), 'utf8');
}

// Hard marker: the file exists AND contains the symbol/test/marker text.
// Claims are derived from these, never from a file name alone.
function hasText(rel, needle) {
  return file(rel) && readText(rel).includes(needle);
}

function hasAllText(rel, needles) {
  return needles.every((needle) => hasText(rel, needle));
}

function hasAnyText(rel, needles) {
  return needles.some((needle) => hasText(rel, needle));
}

function statusFromMarkers(present, complete) {
  if (complete) {
    return 'IMPLEMENTED';
  }
  return present ? 'PARTIAL' : 'ABSENT';
}

const JETBRAINS_PARITY_MATRIX = 'target/certification/jetbrains-parity.json';
const VSCODE_PANEL_SOURCES = [
  'apps/vscode/src/webview.ts',
  'apps/vscode/media/chat.js',
  'apps/vscode/media/chat.css',
];

function readJson(rel) {
  try {
    return JSON.parse(readFileSync(resolve(ROOT, rel), 'utf8'));
  } catch {
    return null;
  }
}

function headCommit() {
  try {
    return execFileSync('git', ['rev-parse', 'HEAD'], { cwd: ROOT, encoding: 'utf8' }).trim();
  } catch {
    return null;
  }
}

const HEAD = headCommit();

// The Faktor-owned UI migration retired every vendored upstream UI corpus and
// its pin manifest: ui/ carries only the historical attribution directory and
// no compat/ corpus may reappear. A reappearing vendored tree is a hard
// self-check failure, never a label change.
function retiredVendoredArtifacts() {
  const problems = [];
  const uiEntries = dir('ui')
    ? readdirSync(resolve(ROOT, 'ui')).sort()
    : [];
  if (JSON.stringify(uiEntries) !== JSON.stringify(['LICENSES'])) {
    problems.push(
      `ui/ must carry only the historical attribution directory (found: ${uiEntries.join(', ') || 'nothing'})`,
    );
  }
  if (dir('compat')) {
    problems.push('compat/ must not carry a pinned upstream corpus');
  }
  return problems;
}

// ------------------------------------------------------------- derivations

// The executable JetBrains parity artifact (one report for both axes):
// `{schema, commit, rows[], behavioral{passed,total}, visual{passed,total,
//  method}}`. A label is only evidence when the report is a real object bound
// to the exact HEAD and the axis it names is fully passed by its own checks.
// The smoke suite's green output is never consulted.
function jetbrainsMatrix(rel) {
  const matrix = readJson(rel);
  if (!matrix || typeof matrix !== 'object' || matrix.schema !== 'faktor-jetbrains-parity/v1') {
    return null;
  }
  return matrix;
}

function behavioralMatrixResult(rel) {
  const matrix = jetbrainsMatrix(rel);
  const label = 'JetBrains behavioral parity';
  if (matrix === null) {
    return {
      status: 'PARTIAL',
      detail: `${label}: no executable parity matrix at ${rel} (smoke suites are not parity results)`,
    };
  }
  const bound = HEAD !== null && matrix.commit === HEAD;
  const rows = Array.isArray(matrix.rows) ? matrix.rows : [];
  const behavioral = matrix.behavioral || {};
  const allPassed =
    rows.length > 0 &&
    rows.every((row) => row && row.status === 'passed') &&
    behavioral.passed === rows.length &&
    behavioral.total === rows.length;
  if (matrix.status === 'passed' && bound && allPassed) {
    return {
      status: 'IMPLEMENTED',
      detail: `${label}: ${rows.length}/${rows.length} behavioral rows passed at ${matrix.commit}`,
    };
  }
  const reason = !bound
    ? `matrix commit ${matrix.commit} != HEAD ${HEAD || 'unknown'}`
    : `${behavioral.passed ?? 0}/${behavioral.total ?? rows.length} behavioral rows passed (status=${matrix.status})`;
  return { status: 'PARTIAL', detail: `${label}: ${reason}` };
}

// Visual parity requires a REAL rendered comparison: the matrix must record a
// render-based method, a pinned baseline and a per-panel digest pass count
// that matches its total. A report without those fields stays PARTIAL.
function visualMatrixResult(rel) {
  const matrix = jetbrainsMatrix(rel);
  const label = 'JetBrains visual parity';
  if (matrix === null) {
    return {
      status: 'PARTIAL',
      detail: `${label}: no executable parity matrix at ${rel} (smoke suites are not parity results)`,
    };
  }
  const bound = HEAD !== null && matrix.commit === HEAD;
  const visual = matrix.visual || {};
  const method = typeof visual.method === 'string' ? visual.method : '';
  const rendered = method.includes('render');
  const panels = Array.isArray(visual.panels) ? visual.panels : [];
  const allPanelsPassed =
    panels.length > 0 && panels.every((panel) => panel && panel.status === 'passed');
  const countsMatch =
    Number.isInteger(visual.passed) &&
    Number.isInteger(visual.total) &&
    visual.total > 0 &&
    visual.passed === visual.total &&
    panels.length === visual.total;
  const ok = bound && visual.status === 'passed' && rendered && allPanelsPassed && countsMatch;
  const reason = !bound
    ? `matrix commit ${matrix.commit} != HEAD ${HEAD || 'unknown'}`
    : !rendered
      ? `visual method is not a rendered comparison (method=${method || 'missing'})`
      : `${visual.passed ?? 0}/${visual.total ?? panels.length} rendered panels passed (status=${visual.status})`;
  return {
    status: ok ? 'IMPLEMENTED' : 'PARTIAL',
    detail: ok
      ? `${label}: ${panels.length}/${panels.length} rendered panels match the pinned baselines (${method}) at ${matrix.commit}`
      : `${label}: ${reason}`,
  };
}

// The Faktor-owned VS Code chat panel: hand-written panel sources, the real
// extension provider and the executable selftest/VSIX verification markers.
function vscodePanelStatus() {
  const present = VSCODE_PANEL_SOURCES.every((rel) => file(rel));
  if (!present) {
    return { status: 'ABSENT', detail: 'VS Code Faktor panel sources are absent' };
  }
  const markers =
    hasText('apps/vscode/src/webview.ts', 'class ChatViewProvider') &&
    hasText('apps/vscode/src/webview.ts', "'chat.js'") &&
    hasText('apps/vscode/media/chat.js', 'acquireVsCodeApi') &&
    hasAnyText('apps/vscode/scripts/selftest.mjs', ['packagedLayoutTests', 'SELFTEST OK']) &&
    hasText('apps/vscode/scripts/verify-vsix.mjs', 'PANEL_MARKERS');
  return {
    status: markers ? 'IMPLEMENTED' : 'PARTIAL',
    detail: markers
      ? 'Faktor-owned chat panel: provider + media panel + selftest/VSIX verification'
      : 'Faktor-owned chat panel sources exist but a provider/selftest marker is missing',
  };
}

const SURFACES = {
  vscode_native_client: () => ({
    status: statusFromMarkers(
      file('apps/vscode/src/nativeClient.ts'),
      hasAllText('apps/vscode/src/nativeClient.ts', ['export class', 'validateSessionCreated']) &&
        hasText('apps/vscode/scripts/selftest.mjs', 'assert'),
    ),
    evidence: ['apps/vscode/src/nativeClient.ts', 'apps/vscode/scripts/selftest.mjs'],
  }),
  vscode_webview: () => ({
    ...vscodePanelStatus(),
    evidence: [
      'apps/vscode/src/webview.ts',
      'apps/vscode/media/chat.js',
      'apps/vscode/media/chat.css',
      'apps/vscode/scripts/selftest.mjs',
      'apps/vscode/scripts/verify-vsix.mjs',
    ],
  }),
  jetbrains_native_bridge: () => ({
    status: statusFromMarkers(
      file('apps/jetbrains/backend/src/main/kotlin/dev/faktor/backend/NativeClient.kt'),
      hasText('apps/jetbrains/backend/src/main/kotlin/dev/faktor/backend/NativeClient.kt', 'class NativeClient') &&
        hasText('apps/jetbrains/backend/src/main/kotlin/dev/faktor/backend/NativeEventStream.kt', 'class NativeEventStream') &&
        hasText('apps/jetbrains/backend/src/test/kotlin/dev/faktor/backend/NativeClientTest.kt', 'NATIVE SMOKE PASS'),
    ),
    evidence: [
      'apps/jetbrains/backend/src/main/kotlin/dev/faktor/backend/NativeClient.kt',
      'apps/jetbrains/backend/src/main/kotlin/dev/faktor/backend/NativeEventStream.kt',
      'apps/jetbrains/backend/src/test/kotlin/dev/faktor/backend/NativeClientTest.kt',
      'apps/jetbrains/compile-and-smoke.sh',
    ],
  }),
  // The Faktor-owned frontend OPERATES (panels, plugin descriptor, Gradle
  // build, smoke assertions): that claim does not depend on the upstream
  // assets being present.
  jetbrains_frontend: () => {
    const markers =
      hasText('apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'class FaktorChatPanel') &&
      hasText('apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml', '<id>') &&
      hasText('apps/jetbrains/frontend/build.gradle.kts', 'org.jetbrains.intellij') &&
      hasText('apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/FrontendSmoke.kt', 'FRONTEND SMOKE PASS');
    return {
      status: statusFromMarkers(file('apps/jetbrains/frontend/build.gradle.kts'), markers),
      evidence: [
        'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt',
        'apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml',
        'apps/jetbrains/frontend/build.gradle.kts',
        'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/FrontendSmoke.kt',
      ],
    };
  },
  // Behavioral/visual matrices exist ONLY as executable results bound to the
  // exact HEAD. The kotlinc/daemon smokes are regression tests, not matrix
  // results, and never move these labels.
  jetbrains_behavioral_parity: () => ({
    ...behavioralMatrixResult(JETBRAINS_PARITY_MATRIX),
    evidence: [JETBRAINS_PARITY_MATRIX, 'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParitySmoke.kt'],
  }),
  jetbrains_visual_parity: () => ({
    ...visualMatrixResult(JETBRAINS_PARITY_MATRIX),
    evidence: [JETBRAINS_PARITY_MATRIX, 'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParityMatrix.kt'],
  }),
  // ui_parity: the Faktor-owned UI's executable axes (the VS Code panel
  // selftest/VSIX gates + the JetBrains behavioral/visual matrices). No
  // vendored or pinned corpus participates.
  ui_parity: () => {
    const axes = {
      vscode_panel: vscodePanelStatus(),
      jetbrains_behavioral: behavioralMatrixResult(JETBRAINS_PARITY_MATRIX),
      jetbrains_visual: visualMatrixResult(JETBRAINS_PARITY_MATRIX),
    };
    const statuses = Object.values(axes).map((axis) => axis.status);
    let status = 'BLOCKED_EXTERNAL';
    if (statuses.every((s) => s === 'IMPLEMENTED')) {
      status = 'IMPLEMENTED';
    } else if (statuses.some((s) => s !== 'ABSENT')) {
      // At least one executable axis exists; the rest are open.
      status = 'PARTIAL';
    }
    return {
      status,
      evidence: [
        ...VSCODE_PANEL_SOURCES,
        JETBRAINS_PARITY_MATRIX,
        'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/JetBrainsParityMatrix.kt',
      ].filter((rel) => file(rel) || dir(rel)),
      detail: Object.entries(axes)
        .map(([axis, result]) => `${axis}=${result.status}`)
        .join(' '),
    };
  },
  acp_subset: () => ({
    status: statusFromMarkers(
      file('crates/acp/src/lib.rs'),
      hasText('crates/acp/src/protocol.rs', 'AcpMethod::Initialize') &&
        hasText('crates/acp/tests/interop.rs', 'fn ') &&
        hasText('tests/acp-official/Cargo.toml', 'agent-client-protocol'),
    ),
    evidence: [
      'crates/acp/src/lib.rs',
      'crates/acp/src/protocol.rs',
      'crates/acp/tests/interop.rs',
      'tests/acp-official',
    ],
  }),
  // Provider family claims: the Responses codec is real only when dispatch,
  // serializer, stream parser and the adversarial tests are all present.
  openai_responses: () => ({
    status: statusFromMarkers(
      file('crates/openai/src/lib.rs'),
      hasAllText('crates/openai/src/lib.rs', [
        'OpenAiFamily::Responses',
        'pub fn responses_body(',
        'pub fn responses_stream(',
        'fn responses_stream_parses_text_reasoning_and_tool_calls(',
        'fn responses_no_chunks_survive_any_terminal_event(',
      ]),
    ),
    evidence: ['crates/openai/src/lib.rs'],
  }),
  // Windows containment: job-object symbols in the supervisor AND the
  // ConPTY spawn-suspended/assign/resume order in faktor-pty.
  windows_job_containment: () => ({
    status: statusFromMarkers(
      file('crates/winjob/src/lib.rs'),
      hasAllText('crates/winjob/src/lib.rs', [
        'CreateJobObjectW',
        'SetInformationJobObject',
        'AssignProcessToJobObject',
        'JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE',
      ]) &&
        hasAllText('crates/pty/src/windows.rs', [
          'CREATE_SUSPENDED',
          'assign_strict',
          'fn spawn_assigns_the_child_to_the_kill_on_close_job_before_returning(',
        ]),
    ),
    evidence: ['crates/winjob/src/lib.rs', 'crates/pty/src/windows.rs'],
  }),
  // Certification evidence chain: the verifier, the schema and the v2 lane
  // markers must all be present, and certify-local must consume the verifier.
  certification_evidence_chain: () => ({
    status: statusFromMarkers(
      file('scripts/certification/evidence.mjs'),
      hasAllText('scripts/certification/evidence.mjs', [
        'faktor-cert-evidence/v1',
        'verify-markers',
        'faktor-woodpecker-lane/v2',
      ]) &&
        hasText('scripts/certification/evidence.schema.json', 'faktor-cert-evidence/v1') &&
        hasText('scripts/certify-local.sh', 'evidence_gate') &&
        hasText('.woodpecker/untrusted/pr.yaml', 'faktor-woodpecker-lane/v2'),
    ),
    evidence: [
      'scripts/certification/evidence.mjs',
      'scripts/certification/evidence.schema.json',
      'scripts/certify-local.sh',
      '.woodpecker/untrusted/pr.yaml',
    ],
  }),
};

// ------------------------------------------- capability certification axes
//
// The five axes of every capability are DERIVED, never written:
//
//   * wired                 — the production construction markers exist (the
//                             daemon really assembles this capability);
//   * adapter_certified     — the adapter/contract-mock test exists;
//   * daemon_e2e_certified  — a `tests/production-wiring` test drives the
//                             executable's own `build_daemon_core` graph for
//                             this capability (the suite is the source);
//   * platforms_certified   — the Woodpecker lanes whose commands really run
//                             the workspace tests (derived from the workflow
//                             files, not asserted);
//   * external_dependency   — `faked` when the certification substitutes an
//                             external seam (loopback/fake server, fake
//                             transport) for this capability's tests, `none`
//                             when the capability runs over durable local
//                             state only.
//
// tests/production-wiring is the daemon-e2e source: its tests compile the
// faktor-cli binary as a library and call `build_production_graph*`, so a
// certified axis is an executed daemon path, never a hand-built rig.

const PW_ROOT = 'tests/production-wiring';
const PW_TESTS = `${PW_ROOT}/tests`;

/** The exact test file(s) whose fn must exist for a daemon-e2e axis. */
function hasTestFunction(rel, fn) {
  return hasText(rel, `fn ${fn}(`);
}

function anyProbe(probes) {
  return probes.some(([rel, needle]) =>
    needle === undefined ? hasText(rel, 'fn ') : hasText(rel, needle),
  );
}

/** Platform lanes that run the workspace tests (derived from the trusted
 *  workflow's lane blocks, so an unexecuted platform can never appear). */
function platformsCertified() {
  const workflow = file('.woodpecker/trusted/trusted.yaml')
    ? readText('.woodpecker/trusted/trusted.yaml')
    : '';
  const platforms = new Set();
  for (const block of workflow.split(/\n  - name: /).slice(1)) {
    if (!/cargo test --workspace\b/.test(block)) {
      continue;
    }
    for (const match of block.matchAll(/platform: (linux|darwin|windows)\//g)) {
      platforms.add(match[1]);
    }
  }
  return [...platforms].sort();
}

const AXIS_SPECS = {
  acquisition_planner: {
    wired: [
      ['crates/cli/src/daemon/builder.rs', 'build_daemon_with_acquisition_planner'],
      ['crates/cli/src/daemon/builder.rs', 'open_commerce_service_with_planner'],
    ],
    adapter_certified: [
      [`${PW_TESTS}/marketplace_auth.rs`, 'fn signed_alibaba_request_is_verified_by_the_contract_mock('],
    ],
    daemon_e2e_certified: [
      [`${PW_TESTS}/commerce.rs`, 'fn daemon_tool_quote_runs_the_production_planner_decisions('],
      [`${PW_TESTS}/commerce.rs`, 'struct SpyPlanner'],
    ],
  },
  identity_cache: {
    wired: [
      ['crates/commerce/src/service.rs', 'fn cache_identity('],
      ['crates/commerce/src/service.rs', 'fn observation_identity('],
    ],
    adapter_certified: [
      [`${PW_TESTS}/commerce.rs`, 'fn configured_account_scope_reaches_cache_identity('],
    ],
    daemon_e2e_certified: [
      [`${PW_TESTS}/commerce.rs`, 'fn configured_account_scope_reaches_cache_identity('],
      [`${PW_TESTS}/commerce.rs`, 'fn authenticated_prices_never_enter_the_public_cache('],
    ],
  },
  vision: {
    wired: [
      ['crates/provider/src/lib.rs', 'pub const SUPPORTED_IMAGE_MIMES'],
      ['crates/provider/src/lib.rs', 'pub struct ContentPart'],
    ],
    adapter_certified: [
      [`${PW_TESTS}/vision.rs`, 'fn adapter_serializes_ordered_media_parts_byte_exact('],
    ],
    daemon_e2e_certified: [
      [`${PW_TESTS}/vision.rs`, 'fn vision_turn_routes_through_the_agent_and_sends_ordered_media('],
      [`${PW_TESTS}/vision.rs`, 'fn non_vision_model_refuses_before_dispatch('],
    ],
  },
  retrieval: {
    wired: [
      ['crates/agent/src/runtime.rs', 'fn index_service('],
      ['crates/cli/src/daemon/builder.rs', 'IndexService::open_with_supervisor('],
    ],
    adapter_certified: [
      [
        'crates/agent/src/runtime/retrieval_tests.rs',
        'fn configured_embedder_fuses_semantically_matched_evidence_into_the_request(',
      ],
    ],
    daemon_e2e_certified: null, // probed generically: see retrievalDaemonE2e()
  },
  scm: {
    wired: [
      ['crates/cli/src/daemon/wiring.rs', 'fn wire_completion_scm('],
      ['crates/scm/src/completion.rs', 'pub struct GitHubCompletionScm'],
    ],
    adapter_certified: [
      [
        `${PW_TESTS}/completion_scm_adapter_contract.rs`,
        'fn manual_completion_scm_adapter_contract_and_embedded_host_injection(',
      ],
    ],
    daemon_e2e_certified: [
      [`${PW_TESTS}/completion_scm.rs`, 'fn completion_pr_goes_through_the_production_github_app_adapter('],
    ],
  },
  jobs: {
    wired: [
      ['crates/commerce/src/service.rs', 'pub struct CommerceSourceService'],
    ],
    adapter_certified: null,
    daemon_e2e_certified: [
      [`${PW_TESTS}/commerce.rs`, 'fn job_status_is_requester_scoped('],
    ],
  },
};

/** The daemon-e2e retrieval certification: any `tests/production-wiring`
 *  test file that both reaches the production builder and names a retrieval
 *  test function. A sibling may land it in its own file; when absent the
 *  axis stays false with an explicit reason (never a guessed label). */
function retrievalDaemonE2e() {
  if (!dir(PW_TESTS)) {
    return { ok: false, detail: 'tests/production-wiring/tests is absent' };
  }
  for (const name of readdirSync(resolve(ROOT, PW_TESTS)).sort()) {
    if (!name.endsWith('.rs')) {
      continue;
    }
    const rel = `${PW_TESTS}/${name}`;
    const src = readText(rel);
    if (
      src.includes('build_production_graph') &&
      /fn [a-z0-9_]*retriev[a-z0-9_]*\(/i.test(src) &&
      /evidence/i.test(src)
    ) {
      return { ok: true, evidence: [rel], detail: `daemon-core retrieval test in ${rel}` };
    }
  }
  return {
    ok: false,
    detail:
      'no tests/production-wiring test drives retrieval through build_production_graph (the ' +
      'daemon-core retrieval behaviors are certified only at the agent runtime today)',
  };
}

/** The external seams a capability's certification substitutes, phrased as a
 *  suite FACT: `faked` when the tests use a loopback/fake server or fake
 *  transport, `none` when the capability's own authority is durable local
 *  state. */
function externalDependency(files) {
  const markers = ['fake', 'Fake', 'mock', 'Mock', 'loopback', '127.0.0.1'];
  const seams = files.filter(
    (rel) =>
      rel.startsWith(`${PW_ROOT}/`) &&
      file(rel) &&
      markers.some((marker) => readText(rel).includes(marker)),
  );
  return seams.length > 0
    ? { value: 'faked', detail: `certification substitutes external seams (${seams.join(', ')})` }
    : { value: 'none', detail: 'certification uses no external seam' };
}

function buildAxes() {
  const platforms = platformsCertified();
  const axes = {};
  for (const [key, spec] of Object.entries(AXIS_SPECS)) {
    const wired = anyProbe(spec.wired);
    const adapterCertified = spec.adapter_certified === null ? false : anyProbe(spec.adapter_certified);
    let daemonE2e = false;
    let daemonE2eDetail = 'no daemon-e2e probe';
    let daemonE2eFiles = [];
    if (spec.daemon_e2e_certified === null) {
      const probe = retrievalDaemonE2e();
      daemonE2e = probe.ok;
      daemonE2eDetail = probe.detail;
      daemonE2eFiles = probe.evidence || [];
    } else {
      daemonE2e = anyProbe(spec.daemon_e2e_certified);
      daemonE2eFiles = [...new Set(spec.daemon_e2e_certified.map(([rel]) => rel))];
      daemonE2eDetail = daemonE2e
        ? `production-wiring tests: ${daemonE2eFiles.join(', ')}`
        : 'no tests/production-wiring test drives the production builder for this capability';
    }
    const testFiles = [
      ...new Set([
        ...(spec.adapter_certified || []).map(([rel]) => rel),
        ...daemonE2eFiles,
      ]),
    ];
    const external = externalDependency(testFiles);
    axes[key] = {
      wired,
      adapter_certified: adapterCertified,
      daemon_e2e_certified: daemonE2e,
      platforms_certified: daemonE2e || (wired && adapterCertified) ? platforms : [],
      external_dependency: external.value,
      detail: daemonE2eDetail,
      external_dependency_detail: external.detail,
      evidence: [
        ...new Set([
          ...spec.wired.filter(([rel]) => file(rel)).map(([rel]) => rel),
          ...testFiles.filter((rel) => file(rel)),
        ]),
      ],
    };
  }
  return axes;
}

// ------------------------------------------------- README capability claims
//
// README carries ONE machine-checkable claims table between the
// `<-- capability-axes:start/end -->` markers: the five derived fields per
// capability, in a fixed order. A hand-written binary/status claim outside
// this table is rejected: the fields are compared to the manifest above, so
// prose can never drift from the tree.

const CLAIMS_START = '<!-- capability-axes:start -->';
const CLAIMS_END = '<!-- capability-axes:end -->';
const CLAIMS_DOCS = ['README.md', 'docs/certification.md'];

function parseClaimsBlock(doc) {
  const lines = readText(doc).split('\n');
  const start = lines.findIndex((line) => line.includes(CLAIMS_START));
  const end = lines.findIndex((line) => line.includes(CLAIMS_END));
  if (start === -1 && end === -1) {
    return { lines, rows: null, start: -1, end: -1 };
  }
  if (start === -1 || end === -1 || end < start) {
    throw new Error(`${doc}: unbalanced capability-axes claim markers`);
  }
  const rows = new Map();
  for (const line of lines.slice(start + 1, end)) {
    const match = /^\|\s*`([a-z0-9_]+)`\s*\|(.+)\|\s*$/.exec(line);
    if (!match) {
      continue;
    }
    const cells = match[2].split('|').map((cell) => cell.trim());
    if (cells.length !== 5) {
      throw new Error(
        `${doc}: capability '${match[1]}' must carry the five derived fields ` +
          '(wired, adapter_certified, daemon_e2e_certified, platforms_certified, external_dependency)',
      );
    }
    rows.set(match[1], {
      wired: cells[0],
      adapter_certified: cells[1],
      daemon_e2e_certified: cells[2],
      platforms_certified: cells[3],
      external_dependency: cells[4],
    });
  }
  return { lines, rows, start, end };
}

/** The only place a binary capability claim may appear is the marked table:
 *  a `wired=true` / `adapter_certified: false` / ... shape in prose is
 *  rejected, so a hand-written claim can never drift from the manifest. */
const BINARY_CLAIM = /\b(wired|adapter_certified|daemon_e2e_certified|platforms_certified|external_dependency)\b\s*[:=]\s*(true|false)/i;

function manualBinaryProseErrors(doc, lines, start, end) {
  const errors = [];
  lines.forEach((line, index) => {
    if (start !== -1 && index > start && index < end) {
      return;
    }
    if (BINARY_CLAIM.test(line)) {
      errors.push(
        `${doc}:${index + 1} carries a manual binary capability claim outside the machine-checked ` +
          `table (${line.trim().slice(0, 80)}); move it into the capability-axes block`,
      );
    }
  });
  return errors;
}

function claimsDriftErrors(axes) {
  const errors = [];
  for (const doc of CLAIMS_DOCS) {
    if (!file(doc)) {
      continue;
    }
    let parsed;
    try {
      parsed = parseClaimsBlock(doc);
    } catch (error) {
      errors.push(String(error.message || error));
      continue;
    }
    errors.push(...manualBinaryProseErrors(doc, parsed.lines, parsed.start, parsed.end));
    const rows = parsed.rows;
    if (rows === null) {
      continue;
    }
    for (const [key, axis] of Object.entries(axes)) {
      const row = rows.get(key);
      if (row === undefined) {
        errors.push(`${doc}: capability claims table is missing '${key}'`);
        continue;
      }
      const expected = {
        wired: String(axis.wired),
        adapter_certified: String(axis.adapter_certified),
        daemon_e2e_certified: String(axis.daemon_e2e_certified),
        platforms_certified: axis.platforms_certified.join(','),
        external_dependency: axis.external_dependency,
      };
      for (const [field, want] of Object.entries(expected)) {
        if (row[field] !== want) {
          errors.push(
            `${doc}: ${key}.${field} claims '${row[field]}' but the tree derives '${want}'`,
          );
        }
      }
    }
    for (const key of rows.keys()) {
      if (!(key in axes)) {
        errors.push(`${doc}: claims table lists unknown capability '${key}'`);
      }
    }
  }
  return errors;
}

function buildManifest() {
  let commit = 'unknown';
  try {
    commit = execFileSync('git', ['rev-parse', 'HEAD'], {
      cwd: ROOT,
      encoding: 'utf8',
    }).trim();
  } catch {
    // A tree without git metadata still yields a usable (if unbound) manifest.
  }
  const surfaces = {};
  for (const [key, derive] of Object.entries(SURFACES)) {
    const { status, evidence, detail } = derive();
    surfaces[key] = {
      status,
      ...(detail ? { detail } : {}),
      evidence: evidence.filter((rel) => file(rel) || dir(rel)),
    };
  }
  return {
    schema: 'faktor-capabilities-manifest/v1',
    commit,
    generated_from: 'repository files/scripts (scripts/capabilities-manifest.mjs)',
    surfaces,
    axes: buildAxes(),
  };
}

// ---------------------------------------------------------- self-check
//
// The derivations above are only as honest as their probes. This check
// fails loudly when:
//   (a) the file()/dir() probes stop distinguishing files from directories;
//   (b) a matrix label is claimed without its artifact: the JetBrains
//       parity labels require a real HEAD-bound matrix report, the Faktor
//       panel marker set must exist, and ui_parity=IMPLEMENTED requires
//       every executable axis. A reappearing vendored corpus fails too.
function selfCheck(manifest) {
  const problems = [];
  if (dir('scripts') !== true || dir('scripts/capabilities-manifest.mjs') !== false) {
    problems.push(
      'file()/dir() probes disagree with the tree (scripts/ is a directory, scripts/capabilities-manifest.mjs is a regular file)',
    );
  } else if (file('scripts/capabilities-manifest.mjs') !== true) {
    problems.push('file() probe disagrees with the tree for scripts/capabilities-manifest.mjs');
  }
  problems.push(...retiredVendoredArtifacts());
  const webview = manifest.surfaces.vscode_webview.status;
  const frontend = manifest.surfaces.jetbrains_frontend.status;
  const behavioral = manifest.surfaces.jetbrains_behavioral_parity.status;
  const visual = manifest.surfaces.jetbrains_visual_parity.status;
  const uiParity = manifest.surfaces.ui_parity.status;

  if (webview === 'IMPLEMENTED' && !vscodePanelStatus().detail.includes('Faktor-owned')) {
    problems.push('vscode_webview claims IMPLEMENTED without the Faktor-owned panel markers');
  }
  if (frontend === 'IMPLEMENTED' && !hasText('apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'class FaktorChatPanel')) {
    problems.push('jetbrains_frontend claims IMPLEMENTED without the Faktor panel class');
  }
  if (behavioral === 'IMPLEMENTED' && jetbrainsMatrix(JETBRAINS_PARITY_MATRIX) === null) {
    problems.push(
      'jetbrains_behavioral_parity claims IMPLEMENTED without an executable matrix report',
    );
  }
  if (visual === 'IMPLEMENTED' && visualMatrixResult(JETBRAINS_PARITY_MATRIX).status !== 'IMPLEMENTED') {
    problems.push(
      'jetbrains_visual_parity claims IMPLEMENTED without a rendered comparison against pinned baselines',
    );
  }
  if (uiParity === 'IMPLEMENTED') {
    const axes = [
      vscodePanelStatus().status,
      behavioralMatrixResult(JETBRAINS_PARITY_MATRIX).status,
      visualMatrixResult(JETBRAINS_PARITY_MATRIX).status,
    ];
    if (axes.some((status) => status !== 'IMPLEMENTED')) {
      problems.push(
        `ui_parity claims IMPLEMENTED but an executable axis is not: ${axes.join(', ')}`,
      );
    }
  }
  if (problems.length > 0) {
    for (const problem of problems) {
      console.error(`capabilities self-check: ${problem}`);
    }
    process.exit(1);
  }
}

// ---------------------------------------------------------- docs drift check

// The doc's capability table rows carry a backticked key and an uppercase
// status: `| \`vscode_webview\` | IMPLEMENTED | ... |`.
const DOC_ROW = /^\|\s*`([a-z0-9_]+)`\s*\|\s*([A-Z_]+)\s*\|/;

function parseDocTable() {
  if (!existsSync(DOC_PATH)) {
    throw new Error(`${DOC_PATH} does not exist`);
  }
  const rows = new Map();
  for (const line of readFileSync(DOC_PATH, 'utf8').split('\n')) {
    const match = DOC_ROW.exec(line);
    if (!match) {
      continue;
    }
    const [, key, status] = match;
    if (rows.has(key)) {
      throw new Error(`docs/certification.md lists capability '${key}' twice`);
    }
    rows.set(key, status);
  }
  return rows;
}

function checkDocs(manifest) {
  const rows = parseDocTable();
  const errors = [];
  for (const [key, surface] of Object.entries(manifest.surfaces)) {
    if (!rows.has(key)) {
      errors.push(`docs/certification.md is missing a capability row for '${key}'`);
    } else if (rows.get(key) !== surface.status) {
      errors.push(
        `docs/certification.md says ${key}=${rows.get(key)} but the tree derives ${surface.status}`,
      );
    }
  }
  for (const key of rows.keys()) {
    if (!(key in manifest.surfaces)) {
      errors.push(`docs/certification.md lists unknown capability '${key}'`);
    }
  }
  errors.push(...implementedClaimErrors());
  errors.push(...claimsDriftErrors(manifest.axes));
  if (errors.length > 0) {
    for (const error of errors) {
      console.error(`capabilities drift: ${error}`);
    }
    console.error(
      'fix docs/certification.md (or the derivation) so the table matches target/certification/capabilities.json',
    );
    process.exit(1);
  }
}

// A doc row that claims IMPLEMENTED must carry at least one real artifact:
// a backticked path/symbol that exists in the tree. Prose-only claims fail.
const IMPLEMENTED_CLAIM_DOCS = ['docs/certification.md', 'README.md'];

function implementedClaimErrors() {
  const errors = [];
  for (const doc of IMPLEMENTED_CLAIM_DOCS) {
    if (!file(doc)) {
      continue;
    }
    const lines = readText(doc).split('\n');
    lines.forEach((line, index) => {
      if (!/^\s*\|/.test(line) || !/\bIMPLEMENTED\b/.test(line)) {
        return;
      }
      const tokens = [...line.matchAll(/`([^`]+)`/g)]
        .map((match) => match[1].trim())
        .filter((token) => /[./]/.test(token) && !/\s/.test(token));
      if (tokens.length === 0) {
        errors.push(
          `${doc}:${index + 1} claims IMPLEMENTED without a backticked code/test evidence path`,
        );
        return;
      }
      if (!tokens.some((token) => file(token) || dir(token))) {
        errors.push(
          `${doc}:${index + 1} claims IMPLEMENTED but none of its evidence paths exist: ${tokens.join(', ')}`,
        );
      }
    });
  }
  return errors;
}

// --------------------------------------------------------------------- main

const manifest = buildManifest();
selfCheck(manifest);
mkdirSync(resolve(ROOT, OUT_DIR), { recursive: true });
writeFileSync(MANIFEST_PATH, `${JSON.stringify(manifest, null, 2)}\n`);

if (CHECK_CLAIMS) {
  // The docs-sync mode: validate ONLY the README/docs capability-axes claims
  // against the derived fields. Surface labels that depend on executable
  // artifacts (the JetBrains matrices) are checked by the full mode /
  // certify-local, so a host without those artifacts still enforces the
  // capability claims it can derive.
  const errors = claimsDriftErrors(manifest.axes);
  if (errors.length > 0) {
    for (const error of errors) {
      console.error(`capability-axes drift: ${error}`);
    }
    console.error(
      'fix the README/docs capability-axes table (or the derivation) so it matches ' +
        'target/certification/capabilities.json',
    );
    process.exit(1);
  }
  console.log(`capabilities manifest: ${MANIFEST_PATH}`);
  console.log('capability-axes claims: README/docs match the derived manifest.');
  process.exit(0);
}

if (!GENERATE_ONLY) {
  checkDocs(manifest);
}

const summary = Object.entries(manifest.surfaces)
  .map(([key, surface]) => `${key}=${surface.status}`)
  .join(' ');
console.log(`capabilities manifest: ${MANIFEST_PATH}`);
console.log(`surfaces: ${summary}`);
console.log(
  `axes: ${Object.entries(manifest.axes)
    .map(
      ([key, axis]) =>
        `${key}[wired=${axis.wired} adapter=${axis.adapter_certified} ` +
        `daemon_e2e=${axis.daemon_e2e_certified} platforms=${axis.platforms_certified.join('+') || 'none'} ` +
        `external=${axis.external_dependency}]`,
    )
    .join(' ')}`,
);
console.log(
  'capabilities self-check: probes ok; Faktor-owned UI (no vendored corpus) and every label carries its earned status (matrix labels only from executable matrices).',
);
if (!GENERATE_ONLY) {
  console.log('docs/certification.md capability table + README/docs capability-axes claims in sync.');
}
