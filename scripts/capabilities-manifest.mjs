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
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const OUT_DIR = process.env.CAPABILITIES_OUT_DIR || 'target/certification';
const MANIFEST_PATH = resolve(ROOT, OUT_DIR, 'capabilities.json');
const DOC_PATH = resolve(ROOT, 'docs/certification.md');
const GENERATE_ONLY = process.argv.includes('--generate-only');

const file = (rel) => existsSync(resolve(ROOT, rel));
const dir = (rel) => existsSync(resolve(ROOT, rel)) && !file(rel);

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
const JETBRAINS_712_ROOTS = ['ui/kilo-jetbrains-712', 'compat/jetbrains-712'];

// ------------------------------------------------------------- derivations

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

function jetbrains712Vendored() {
  return JETBRAINS_712_ROOTS.some((root) => dir(root));
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
  jetbrains_frontend: () => {
    const markers =
      hasText('apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt', 'class FaktorChatPanel') &&
      hasText('apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml', '<id>') &&
      hasText('apps/jetbrains/frontend/build.gradle.kts', 'org.jetbrains.intellij') &&
      hasText('apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/FrontendSmoke.kt', 'FRONTEND SMOKE PASS');
    // Honest PARTIAL: the Faktor-owned frontend exists and smokes, but the
    // upstream 7.1.2 sources are not vendored, so parity stays partial.
    return {
      status: markers ? (jetbrains712Vendored() ? 'IMPLEMENTED' : 'PARTIAL') : statusFromMarkers(file('apps/jetbrains/frontend/build.gradle.kts'), false),
      evidence: [
        'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt',
        'apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml',
        'apps/jetbrains/frontend/build.gradle.kts',
        'apps/jetbrains/frontend/src/test/kotlin/dev/faktor/frontend/FrontendSmoke.kt',
      ],
    };
  },
  compat_v756: () => ({
    status: statusFromMarkers(
      file('compat/kilo-v756/startup_line.json'),
      hasAllText('compat/kilo-v756/startup_line.json', ['faktor server listening on']) &&
        hasText('compat/kilo-v756/sse_frames.json', '{') &&
        hasText('crates/protocol/src/fixtures.rs', 'compat/kilo-v756') &&
        file('tests/compat/Cargo.toml'),
    ),
    evidence: [
      'compat/kilo-v756',
      'compat/kilo-v756/startup_line.json',
      'compat/kilo-v756/sse_frames.json',
      'crates/protocol/src/fixtures.rs',
      'tests/compat',
    ],
  }),
  ui_parity: () => {
    const vscode = vscodeWebviewStatus();
    const jetbrains712 = jetbrains712Vendored();
    let status = 'BLOCKED_EXTERNAL';
    if (vscode === 'IMPLEMENTED' && jetbrains712) {
      status = 'IMPLEMENTED';
    } else if (vscode !== 'BLOCKED_EXTERNAL' || jetbrains712) {
      status = 'PARTIAL';
    }
    return {
      status,
      evidence: [
        `${VENDORED_WEBVIEW_ROOT}/dist/visual-baseline.json`,
        'ui/upstream.json',
        ...JETBRAINS_712_ROOTS,
      ],
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
    const { status, evidence } = derive();
    surfaces[key] = {
      status,
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
if (!GENERATE_ONLY) {
  console.log('docs/certification.md capability table in sync.');
}
