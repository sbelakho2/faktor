#!/usr/bin/env node
// Machine-readable capability manifest (certification truth audit).
//
// Every status is DERIVED from repository files and scripts — never from
// prose. The generator probes the pinned vendored UI tree, the extension
// sources, the JetBrains backend/frontend sources, the frozen compat
// fixtures and the ACP subset, then writes:
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
const KILO_COMPAT_REPORT = 'target/certification/kilo-compat.json';
const JETBRAINS_BEHAVIORAL_MATRIX = 'target/certification/jetbrains-behavioral-parity.json';
const JETBRAINS_VISUAL_MATRIX = 'target/certification/jetbrains-visual-parity.json';
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

// An executable parity matrix is only evidence when it is a report object
// bound to the exact HEAD and every row passed. Canned/fake-daemon smokes
// are not parity results and are never consulted here.
function matrixResult(rel, label) {
  const matrix = readJson(rel);
  if (!matrix || typeof matrix !== 'object') {
    return {
      status: 'PARTIAL',
      detail: `${label}: no executable parity matrix at ${rel} (smoke suites are not parity results)`,
    };
  }
  const bound = HEAD !== null && matrix.commit === HEAD;
  const rows = Array.isArray(matrix.rows) ? matrix.rows : [];
  const allPassed = rows.length > 0 && rows.every((row) => row && row.status === 'passed');
  if (matrix.status === 'passed' && bound && allPassed) {
    return {
      status: 'IMPLEMENTED',
      detail: `${label}: ${rows.length}/${rows.length} matrix rows passed at ${matrix.commit}`,
    };
  }
  const reason = !bound
    ? `matrix commit ${matrix.commit} != HEAD ${HEAD || 'unknown'}`
    : `${rows.filter((row) => row?.status === 'passed').length}/${rows.length} rows passed (status=${matrix.status})`;
  return { status: 'PARTIAL', detail: `${label}: ${reason}` };
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

// The frozen-upstream client replay is the executable behavioral matrix for
// the v7.5.6 wire surface. IMPLEMENTED only when the report is bound to the
// exact HEAD and responses are N/N; documented divergences keep PARTIAL.
function compatV756Result() {
  const report = readJson(KILO_COMPAT_REPORT);
  const evidence = ['tests/compat', 'compat/kilo-v756/sdk-traces', KILO_COMPAT_REPORT];
  if (!report || report.schema !== 'faktor-kilo-compat/v1') {
    return {
      status: 'PARTIAL',
      evidence: evidence.filter((rel) => file(rel) || dir(rel)),
      detail: 'no usable kilo-compat report: the fixture corpus exists, the executable replay is unproven',
    };
  }
  const responses = report.responses || {};
  const requests = report.requests || {};
  const bound = HEAD !== null && report.commit === HEAD;
  const exact =
    Number.isInteger(responses.total) &&
    responses.total > 0 &&
    responses.passed === responses.total &&
    report.status === 'passed';
  const detail =
    `requests ${requests.passed || 0}/${requests.total || 0}, responses ${responses.passed || 0}/${responses.total || 0} exact, ` +
    `${report.required_divergences || 0} required divergences` +
    (bound ? '' : ` (report commit ${report.commit} != HEAD ${HEAD || 'unknown'})`);
  return {
    status: bound && exact ? 'IMPLEMENTED' : 'PARTIAL',
    evidence: evidence.filter((rel) => file(rel) || dir(rel)),
    detail,
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
    ...matrixResult(JETBRAINS_BEHAVIORAL_MATRIX, 'JetBrains behavioral parity'),
    evidence: [JETBRAINS_BEHAVIORAL_MATRIX, JETBRAINS_712_ROOT, 'ui/upstream.json'],
  }),
  jetbrains_visual_parity: () => ({
    ...matrixResult(JETBRAINS_VISUAL_MATRIX, 'JetBrains visual parity'),
    evidence: [JETBRAINS_VISUAL_MATRIX, JETBRAINS_712_ROOT, 'ui/upstream.json'],
  }),
  // compat_v756 derives from the REQUIRED kilo-compat replay report, never
  // from fixture-file existence: IMPLEMENTED only when the report is bound
  // to this exact HEAD and every response is an exact pass (N/N).
  compat_v756: () => compatV756Result(),
  // ui_parity depends on executable parity results (the VS Code visual
  // render matrix + the frozen-upstream behavioral replay + the JetBrains
  // parity matrices). Vendored files or pinned directories alone never make
  // it IMPLEMENTED.
  ui_parity: () => {
    const axes = {
      vscode_visual: vscodeVisualResult(),
      upstream_behavioral: compatV756Result(),
      jetbrains_behavioral: matrixResult(JETBRAINS_BEHAVIORAL_MATRIX, 'JetBrains behavioral parity'),
      jetbrains_visual: matrixResult(JETBRAINS_VISUAL_MATRIX, 'JetBrains visual parity'),
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
        KILO_COMPAT_REPORT,
        JETBRAINS_BEHAVIORAL_MATRIX,
        JETBRAINS_VISUAL_MATRIX,
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
//       real matrix report, compat_v756=IMPLEMENTED requires a HEAD-bound
//       N/N kilo-compat report, and ui_parity=IMPLEMENTED requires every
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
  const compat = manifest.surfaces.compat_v756.status;
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
  if (behavioral === 'IMPLEMENTED' && readJson(JETBRAINS_BEHAVIORAL_MATRIX) === null) {
    problems.push(
      'jetbrains_behavioral_parity claims IMPLEMENTED without an executable parity matrix report',
    );
  }
  if (visual === 'IMPLEMENTED' && readJson(JETBRAINS_VISUAL_MATRIX) === null) {
    problems.push('jetbrains_visual_parity claims IMPLEMENTED without an executable parity matrix report');
  }
  if (compat === 'IMPLEMENTED') {
    const report = readJson(KILO_COMPAT_REPORT);
    const exact =
      report &&
      report.status === 'passed' &&
      report.commit === HEAD &&
      report.responses &&
      report.responses.total > 0 &&
      report.responses.passed === report.responses.total;
    if (!exact) {
      problems.push(
        'compat_v756 claims IMPLEMENTED without a HEAD-bound kilo-compat report whose responses are N/N',
      );
    }
  }
  if (uiParity === 'IMPLEMENTED') {
    const axes = [
      vscodeVisualResult().status,
      compatV756Result().status,
      matrixResult(JETBRAINS_BEHAVIORAL_MATRIX, 'JetBrains behavioral parity').status,
      matrixResult(JETBRAINS_VISUAL_MATRIX, 'JetBrains visual parity').status,
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
