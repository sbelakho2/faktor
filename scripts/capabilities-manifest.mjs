#!/usr/bin/env node
// Machine-readable capability manifest (certification truth audit).
//
// Every status is DERIVED from repository files and scripts — never from
// prose. The generator probes the pinned vendored UI tree, the extension
// sources, the JetBrains backend/frontend sources, the vendored UI tree
// and the ACP subset, then writes:
//
//   target/certification/capabilities.json
//
// and verifies that the capability table in docs/certification.md carries
// the exact same status for every surface (`--check`, the drift test). A
// mismatch — a stale prose label, a missing row, or a status that no longer
// matches the tree — exits non-zero.
//
// Usage:
//   node scripts/capabilities-manifest.mjs                 generate + drift check
//   node scripts/capabilities-manifest.mjs --generate-only  skip the docs check
//
// Env:
//   CAPABILITIES_OUT_DIR  output directory (default target/certification)

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const OUT_DIR = process.env.CAPABILITIES_OUT_DIR || 'target/certification';
const MANIFEST_PATH = resolve(ROOT, OUT_DIR, 'capabilities.json');
const DOC_PATH = resolve(ROOT, 'docs/certification.md');
const GENERATE_ONLY = process.argv.includes('--generate-only');

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

const VENDORED_WEBVIEW_ROOT = 'ui/kilo-v756-webview';
const JETBRAINS_712_ROOT = 'compat/jetbrains-712';
const JETBRAINS_PARITY_MATRIX = 'target/certification/jetbrains-parity.json';
const VSCODE_VISUAL_MATRIX = `${VENDORED_WEBVIEW_ROOT}/dist/visual-report.json`;

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

// The pinned JetBrains 7.1.2 reference tree: the directory exists AND
// ui/upstream.json carries the per-file hash manifest for it. A bare
// directory is not a pin.
function jetbrainsPin() {
  if (!dir(JETBRAINS_712_ROOT)) {
    return null;
  }
  const pin = readJson('ui/upstream.json')?.jetbrains_712;
  if (!pin || typeof pin !== 'object') {
    return null;
  }
  const hashes = pin.file_hashes;
  if (!hashes || typeof hashes !== 'object' || Object.keys(hashes).length === 0) {
    return null;
  }
  return pin;
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

function vscodeVisualResult() {
  const report = readJson(VSCODE_VISUAL_MATRIX);
  const render = report && report.render;
  if (render && render.status === 'passed') {
    return {
      status: 'IMPLEMENTED',
      detail: `VS Code webview visual render passed (${render.states || '?'} states)`,
    };
  }
  return {
    status: 'PARTIAL',
    detail: 'VS Code webview visual render not passed (report missing/skipped; the required vscode-visual lane owns it)',
  };
}

function vscodeWebviewStatus() {
  if (!file('apps/vscode/src/webview.ts') || !file('apps/vscode/src/kilo-bridge.ts')) {
    return 'ABSENT';
  }
  const markers =
    hasText('apps/vscode/src/kilo-bridge.ts', 'mapKiloFiles') &&
    hasAnyText('apps/vscode/scripts/bridge-selftest.mjs', ['assert', 'throw new Error']);
  if (
    file(`${VENDORED_WEBVIEW_ROOT}/dist/webview.js`) &&
    file(`${VENDORED_WEBVIEW_ROOT}/dist/webview.css`)
  ) {
    // The pinned v7.5.6 webview bundle is vendored AND built; the visual
    // baseline proves the gate ran on this tree.
    return file(`${VENDORED_WEBVIEW_ROOT}/dist/visual-baseline.json`) && markers
      ? 'IMPLEMENTED'
      : 'PARTIAL';
  }
  if (dir(VENDORED_WEBVIEW_ROOT)) {
    return 'PARTIAL';
  }
  // Built-in fallback shell only: the upstream assets are absent.
  return 'BLOCKED_EXTERNAL';
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
    status: vscodeWebviewStatus(),
    evidence: [
      'apps/vscode/src/webview.ts',
      'apps/vscode/src/kilo-bridge.ts',
      `${VENDORED_WEBVIEW_ROOT}/dist/webview.js`,
      `${VENDORED_WEBVIEW_ROOT}/dist/visual-baseline.json`,
      'ui/upstream.json',
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
  // The upstream 7.1.2 reference tree with its per-file SHA-256 pin. VENDORED
  // is a provenance claim, never a parity claim.
  jetbrains_upstream_assets: () => {
    const pin = jetbrainsPin();
    let status = 'ABSENT';
    if (pin) {
      status = 'VENDORED';
    } else if (dir(JETBRAINS_712_ROOT)) {
      status = 'PARTIAL';
    }
    return {
      status,
      evidence: [JETBRAINS_712_ROOT, 'ui/upstream.json', 'compat/jetbrains-712/NOTICE.md'],
    };
  },
  // Behavioral/visual parity exist ONLY as executable parity matrices bound
  // to the exact HEAD. The kotlinc/daemon smokes are regression tests, not
  // parity results, and never move these labels.
  jetbrains_behavioral_parity: () => ({
    ...behavioralMatrixResult(JETBRAINS_PARITY_MATRIX),
    evidence: [JETBRAINS_PARITY_MATRIX, JETBRAINS_712_ROOT, 'ui/upstream.json'],
  }),
  jetbrains_visual_parity: () => ({
    ...visualMatrixResult(JETBRAINS_PARITY_MATRIX),
    evidence: [JETBRAINS_PARITY_MATRIX, JETBRAINS_712_ROOT, 'ui/upstream.json'],
  }),
  // ui_parity depends on executable parity results (the VS Code visual
  // render matrix + the frozen-upstream behavioral replay + the JetBrains
  // parity matrices). Vendored files or pinned directories alone never make
  // it IMPLEMENTED.
  ui_parity: () => {
    const axes = {
      vscode_visual: vscodeVisualResult(),
      jetbrains_behavioral: behavioralMatrixResult(JETBRAINS_PARITY_MATRIX),
      jetbrains_visual: visualMatrixResult(JETBRAINS_PARITY_MATRIX),
    };
    const statuses = Object.values(axes).map((axis) => axis.status);
    let status = 'BLOCKED_EXTERNAL';
    if (statuses.every((s) => s === 'IMPLEMENTED')) {
      status = 'IMPLEMENTED';
    } else if (statuses.some((s) => s !== 'ABSENT')) {
      // At least one executable parity matrix exists; the rest are open.
      status = 'PARTIAL';
    }
    return {
      status,
      evidence: [
        VSCODE_VISUAL_MATRIX,
        JETBRAINS_PARITY_MATRIX,
        JETBRAINS_712_ROOT,
        'ui/upstream.json',
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
        hasText('.woodpecker/pr.yaml', 'faktor-woodpecker-lane/v2'),
    ),
    evidence: [
      'scripts/certification/evidence.mjs',
      'scripts/certification/evidence.schema.json',
      'scripts/certify-local.sh',
      '.woodpecker/pr.yaml',
    ],
  }),
};

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
  };
}

// ---------------------------------------------------------- self-check
//
// The derivations above are only as honest as their probes. This check
// fails loudly when:
//   (a) the file()/dir() probes stop distinguishing files from directories;
//   (b) a provenance/parity label is claimed without its artifact: the
//       JetBrains pin present => assets=VENDORED, parity labels require a
//       real matrix report, and ui_parity=IMPLEMENTED requires every
//       executable parity axis.
function selfCheck(manifest) {
  const problems = [];
  if (dir('scripts') !== true || dir('scripts/capabilities-manifest.mjs') !== false) {
    problems.push(
      'file()/dir() probes disagree with the tree (scripts/ is a directory, scripts/capabilities-manifest.mjs is a regular file)',
    );
  } else if (file('scripts/capabilities-manifest.mjs') !== true) {
    problems.push('file() probe disagrees with the tree for scripts/capabilities-manifest.mjs');
  }
  const pin = jetbrainsPin();
  const assets = manifest.surfaces.jetbrains_upstream_assets.status;
  const frontend = manifest.surfaces.jetbrains_frontend.status;
  const behavioral = manifest.surfaces.jetbrains_behavioral_parity.status;
  const visual = manifest.surfaces.jetbrains_visual_parity.status;
  const uiParity = manifest.surfaces.ui_parity.status;

  if (pin) {
    if (assets !== 'VENDORED') {
      problems.push(
        `jetbrains_upstream_assets is ${assets} although the pinned 7.1.2 corpus ${JETBRAINS_712_ROOT}/ is present`,
      );
    }
    if (frontend !== 'IMPLEMENTED') {
      problems.push(
        `jetbrains_frontend is ${frontend} although the Faktor frontend operates (pin present, markers verified)`,
      );
    }
  } else {
    if (assets === 'VENDORED') {
      problems.push(
        'jetbrains_upstream_assets claims VENDORED but the pinned 7.1.2 corpus is absent/incomplete',
      );
    }
    if (uiParity === 'IMPLEMENTED') {
      problems.push('ui_parity claims IMPLEMENTED but no pinned JetBrains corpus exists');
    }
  }
  if (behavioral === 'IMPLEMENTED' && jetbrainsMatrix(JETBRAINS_PARITY_MATRIX) === null) {
    problems.push(
      'jetbrains_behavioral_parity claims IMPLEMENTED without an executable parity matrix report',
    );
  }
  if (visual === 'IMPLEMENTED' && visualMatrixResult(JETBRAINS_PARITY_MATRIX).status !== 'IMPLEMENTED') {
    problems.push(
      'jetbrains_visual_parity claims IMPLEMENTED without a rendered comparison against pinned baselines',
    );
  }
  if (uiParity === 'IMPLEMENTED') {
    const axes = [
      vscodeVisualResult().status,
      behavioralMatrixResult(JETBRAINS_PARITY_MATRIX).status,
      visualMatrixResult(JETBRAINS_PARITY_MATRIX).status,
    ];
    if (axes.some((status) => status !== 'IMPLEMENTED')) {
      problems.push(
        `ui_parity claims IMPLEMENTED but an executable parity axis is not: ${axes.join(', ')}`,
      );
    }
  }
  if (problems.length > 0) {
    for (const problem of problems) {
      console.error(`capabilities self-check: ${problem}`);
    }
    process.exit(1);
  }
  return pin;
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
const jetbrainsPinned = selfCheck(manifest) !== null;
mkdirSync(resolve(ROOT, OUT_DIR), { recursive: true });
writeFileSync(MANIFEST_PATH, `${JSON.stringify(manifest, null, 2)}\n`);

if (!GENERATE_ONLY) {
  checkDocs(manifest);
}

const summary = Object.entries(manifest.surfaces)
  .map(([key, surface]) => `${key}=${surface.status}`)
  .join(' ');
console.log(`capabilities manifest: ${MANIFEST_PATH}`);
console.log(`surfaces: ${summary}`);
console.log(
  jetbrainsPinned
    ? 'capabilities self-check: probes ok; pinned JetBrains 7.1.2 corpus present and every label carries its earned status (parity labels only from executable matrices).'
    : 'capabilities self-check: probes ok; no pinned JetBrains 7.1.2 corpus (labels stay honest without it).',
);
if (!GENERATE_ONLY) {
  console.log('docs/certification.md capability table in sync.');
}
