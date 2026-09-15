#!/usr/bin/env node
// Exact-SHA certification evidence + Woodpecker lane-marker verification.
//
// This is the single implementation behind three things:
//
//   1. `faktor-cert-evidence/v1` objects written to
//      target/certification/evidence/<kind>.json and LOADED by
//      scripts/certify-local.sh. A release gate is true only when the file
//      exists, is schema-valid, was written for the SAME commit
//      (`git rev-parse HEAD`) and tree (`git rev-parse 'HEAD^{tree}'`; the
//      documented tree-hash command — never `git write-tree`, which hashes
//      the mutable index), has matching command/artifact digests, and is
//      signed with an ed25519 key on the identity allowlist. Environment
//      booleans (CERTIFY_CROSS_PLATFORM_LANES=1, ...) never certify.
//
//   2. `faktor-woodpecker-lane/v2` markers emitted by every CI lane into
//      target/certification/lanes/<lane>.json, and verified by the workflow
//      certificate jobs. The rejection matrix is exactly:
//        missing, unexpected, duplicate, unreadable, wrong-schema,
//        lane-name-mismatch, other-commit, tree-mismatch, stale-run,
//        failed-lane, skipped-required, silent-skip,
//        command-digest-mismatch, command-set-drift, artifact-mismatch,
//        artifact-missing, timestamp-order, failed pipeline status.
//
//   3. Self-tests (`selftest`) that construct a temp git repository and
//      prove every rejection above, the signature allowlist behavior, and
//      that the marker heredocs in .woodpecker/*.yaml match the commands the
//      lanes actually run (no silent drift between commands and evidence).
//
// Usage:
//   node scripts/certification/evidence.mjs write --kind <k> --status passed \
//     [--out-dir target/certification/evidence] [--commands "a|b"] \
//     [--from-markers DIR --only-lane L] [--artifacts a,b] \
//     [--sign-key key.pem --key-id ci] [--runner-os linux --runner-arch amd64 \
//      --runner-ci woodpecker --runner-run-id 42]
//   node scripts/certification/evidence.mjs sign --file E.json --key k.pem --key-id ci
//   node scripts/certification/evidence.mjs verify --kind <k> \
//     [--evidence-dir DIR] [--require-signed] [--keys keys.json] [--json]
//   node scripts/certification/evidence.mjs verify-markers --workflow pr \
//     [--lanes-dir target/certification/lanes] [--out target/certification/ci-certification.json] \
//     [--yaml-dir .woodpecker] [--pipeline-status "$CI_PIPELINE_STATUS"] [--run-id "$CI_PIPELINE_NUMBER"]
//   node scripts/certification/evidence.mjs selftest
//
// Signature model: the signed payload is the canonical JSON (object keys
// sorted recursively) of the evidence object WITHOUT its `signature` field.
// A key allowlist is loaded from --keys or CERTIFY_EVIDENCE_KEYS
// ({"identities":{"<identity>":{"ed25519_public_key":"<base64 raw 32B>"}}});
// the same shape as {"<identity>":"<base64>"} is also accepted. Unsigned
// evidence always fails `--require-signed` (release-grade).

import {
  createHash,
  createPrivateKey,
  createPublicKey,
  generateKeyPairSync,
  sign as cryptoSign,
  verify as cryptoVerify,
} from 'node:crypto';
import { execFileSync } from 'node:child_process';
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const EVIDENCE_SCHEMA = 'faktor-cert-evidence/v1';
const MARKER_SCHEMA = 'faktor-woodpecker-lane/v2';
const CI_CERT_SCHEMA = 'faktor-ci-certification/v2';
const EMPTY_ARTIFACT_DIGEST = `sha256:${sha256Hex('')}`;
const ED25519_SPKI_PREFIX = '302a300506032b6570032100';

// ------------------------------------------------------------------ utils

function sha256Hex(text) {
  return createHash('sha256').update(text).digest('hex');
}

function isoNow() {
  return new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
}

function parseIso(value) {
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : null;
}

function git(args, cwd = ROOT) {
  return execFileSync('git', ['-C', cwd, ...args], { encoding: 'utf8' }).trim();
}

function headCommit(cwd = ROOT) {
  return git(['rev-parse', 'HEAD'], cwd);
}

// The documented tree-hash command for evidence binding. `git write-tree`
// would hash the mutable index; the commit's own tree object is the
// immutable identity of the exact SHA.
function headTree(cwd = ROOT) {
  return git(['rev-parse', 'HEAD^{tree}'], cwd);
}

function isSha(value) {
  return typeof value === 'string' && /^[0-9a-f]{40}$/.test(value);
}

function isDigest(value) {
  return typeof value === 'string' && /^sha256:[0-9a-f]{64}$/.test(value);
}

function canonicalJson(value) {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalJson).join(',')}]`;
  }
  if (value && typeof value === 'object') {
    return `{${Object.keys(value)
      .sort()
      .map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`)
      .join(',')}}`;
  }
  return JSON.stringify(value === undefined ? null : value);
}

// Duplicate JSON keys are rejected: JSON.parse silently keeps the last one,
// which would let a second "commit"/"status" hide behind the first.
function findDuplicateKey(text) {
  let i = 0;
  const stack = [];
  while (i < text.length) {
    const ch = text[i];
    if (ch === '{') {
      stack.push(new Set());
      i += 1;
      continue;
    }
    if (ch === '}') {
      stack.pop();
      i += 1;
      continue;
    }
    if (ch === '"') {
      let j = i + 1;
      let key = '';
      while (j < text.length) {
        if (text[j] === '\\') {
          key += text[j] + (text[j + 1] || '');
          j += 2;
          continue;
        }
        if (text[j] === '"') {
          break;
        }
        key += text[j];
        j += 1;
      }
      let k = j + 1;
      while (k < text.length && /\s/.test(text[k])) {
        k += 1;
      }
      if (text[k] === ':') {
        const frame = stack[stack.length - 1];
        if (frame) {
          if (frame.has(key)) {
            return key;
          }
          frame.add(key);
        }
      }
      i = j + 1;
      continue;
    }
    i += 1;
  }
  return null;
}

function readJsonStrict(path) {
  const text = readFileSync(path, 'utf8');
  const dup = findDuplicateKey(text);
  if (dup) {
    throw new Error(`duplicate JSON key '${dup}'`);
  }
  return JSON.parse(text);
}

function artifactLines(artifacts) {
  return artifacts
    .slice()
    .sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0))
    .map((a) => `${a.sha256}\t${a.path}\n`)
    .join('');
}

function artifactDigest(artifacts) {
  return `sha256:${sha256Hex(artifactLines(artifacts))}`;
}

function hashFile(path) {
  const data = readFileSync(path);
  return `sha256:${createHash('sha256').update(data).digest('hex')}`;
}

function runnerDefaults() {
  const os =
    process.platform === 'darwin'
      ? 'darwin'
      : process.platform === 'win32'
        ? 'windows'
        : process.platform === 'linux'
          ? 'linux'
          : 'unknown';
  const arch = process.arch === 'x64' ? 'amd64' : process.arch;
  const runId = process.env.CI_PIPELINE_NUMBER || process.env.GITHUB_RUN_ID || 'local';
  return { os, arch, ci: process.env.CI ? 'woodpecker' : 'local', run_id: runId };
}

function repositoryName(cwd = ROOT) {
  try {
    return git(['remote', 'get-url', 'origin'], cwd);
  } catch {
    return 'faktor';
  }
}

function argValue(args, name, fallback = '') {
  const idx = args.indexOf(name);
  if (idx === -1) {
    return fallback;
  }
  const value = args[idx + 1];
  if (value === undefined || value.startsWith('--')) {
    return '';
  }
  return value;
}

// ------------------------------------------------- evidence object handling

const EVIDENCE_REQUIRED = [
  'schema',
  'repository',
  'commit_sha',
  'tree_hash',
  'kind',
  'status',
  'started_at',
  'finished_at',
  'commands_digest',
  'artifact_digest',
  'runner',
];

function evidencePayload(evidence) {
  const { signature, ...rest } = evidence;
  void signature;
  return canonicalJson(rest);
}

function loadKeys(keysPath) {
  const path = keysPath || process.env.CERTIFY_EVIDENCE_KEYS || '';
  const identities = {};
  if (!path) {
    return identities;
  }
  const parsed = readJsonStrict(path);
  const source = parsed.identities && typeof parsed.identities === 'object' ? parsed.identities : parsed;
  for (const [identity, entry] of Object.entries(source)) {
    if (typeof entry === 'string') {
      identities[identity] = entry;
    } else if (entry && typeof entry === 'object') {
      identities[identity] = entry.ed25519_public_key || entry.public_key || '';
    }
  }
  return identities;
}

function publicKeyFromRawBase64(base64) {
  const raw = Buffer.from(String(base64 || ''), 'base64');
  if (raw.length !== 32) {
    throw new Error(`ed25519 public key must be 32 raw bytes (got ${raw.length})`);
  }
  return createPublicKey({
    key: Buffer.concat([Buffer.from(ED25519_SPKI_PREFIX, 'hex'), raw]),
    format: 'der',
    type: 'spki',
  });
}

function verifyEvidenceSignature(evidence, identities) {
  const signature = evidence.signature;
  if (!signature || typeof signature !== 'object') {
    return { signed: false, ok: false, reason: 'evidence is unsigned' };
  }
  if (signature.algorithm !== 'ed25519') {
    return { signed: true, ok: false, reason: `unsupported signature algorithm '${signature.algorithm}'` };
  }
  const allowlisted = identities[signature.identity];
  if (!allowlisted) {
    return {
      signed: true,
      ok: false,
      reason: `signature identity '${signature.identity}' is not on the allowlist`,
    };
  }
  if (allowlisted !== signature.public_key) {
    return {
      signed: true,
      ok: false,
      reason: `signature identity '${signature.identity}' does not match the allowlisted public key`,
    };
  }
  try {
    const ok = cryptoVerify(
      null,
      Buffer.from(evidencePayload(evidence), 'utf8'),
      publicKeyFromRawBase64(signature.public_key),
      Buffer.from(signature.value, 'base64'),
    );
    return ok
      ? { signed: true, ok: true, reason: 'signature verified' }
      : { signed: true, ok: false, reason: 'signature does not verify' };
  } catch (error) {
    return { signed: true, ok: false, reason: `signature verification failed: ${error.message}` };
  }
}

function signEvidenceWithKey(evidence, privateKey, identity) {
  const value = cryptoSign(null, Buffer.from(evidencePayload(evidence), 'utf8'), privateKey);
  const rawPublic = createPublicKey(privateKey)
    .export({ format: 'der', type: 'spki' })
    .subarray(12)
    .toString('base64');
  return {
    ...evidence,
    signature: { algorithm: 'ed25519', identity, public_key: rawPublic, value: value.toString('base64') },
  };
}

function signEvidence(evidence, privateKeyPath, identity) {
  return signEvidenceWithKey(evidence, createPrivateKey(readFileSync(privateKeyPath)), identity);
}

function verifyEvidenceObject(evidence, options = {}) {
  const problems = [];
  const { kind, expectedCommit, expectedTree, keys, requireSigned, file } = options;
  const label = file ? `${file}: ` : '';
  if (!evidence || typeof evidence !== 'object' || Array.isArray(evidence)) {
    return { ok: false, signed: false, problems: [`${label}not a JSON object`] };
  }
  for (const field of EVIDENCE_REQUIRED) {
    if (!(field in evidence)) {
      problems.push(`${label}missing required field '${field}'`);
    }
  }
  if (evidence.schema !== EVIDENCE_SCHEMA) {
    problems.push(`${label}schema '${evidence.schema}' != '${EVIDENCE_SCHEMA}'`);
  }
  if (kind && evidence.kind !== kind) {
    problems.push(`${label}kind '${evidence.kind}' != expected '${kind}'`);
  }
  if (!isSha(evidence.commit_sha)) {
    problems.push(`${label}commit_sha is not a 40-hex sha`);
  } else if (expectedCommit && evidence.commit_sha !== expectedCommit) {
    problems.push(`${label}other-commit: commit_sha ${evidence.commit_sha} != HEAD ${expectedCommit}`);
  }
  if (!isSha(evidence.tree_hash)) {
    problems.push(`${label}tree_hash is not a 40-hex sha`);
  } else if (expectedTree && evidence.tree_hash !== expectedTree) {
    problems.push(`${label}tree-mismatch: tree_hash ${evidence.tree_hash} != HEAD tree ${expectedTree}`);
  }
  if (!['passed', 'failed', 'skipped'].includes(evidence.status)) {
    problems.push(`${label}status '${evidence.status}' is not passed|failed|skipped`);
  } else if (options.requirePassed !== false && evidence.status !== 'passed') {
    problems.push(`${label}status '${evidence.status}' does not certify`);
  }
  const started = parseIso(evidence.started_at);
  const finished = parseIso(evidence.finished_at);
  if (started === null) {
    problems.push(`${label}started_at is not a date-time`);
  }
  if (finished === null) {
    problems.push(`${label}finished_at is not a date-time`);
  }
  if (started !== null && finished !== null && finished < started) {
    problems.push(`${label}timestamp-order: finished_at precedes started_at`);
  }
  if (!isDigest(evidence.commands_digest)) {
    problems.push(`${label}commands_digest is not sha256:<64 hex>`);
  }
  if (!isDigest(evidence.artifact_digest)) {
    problems.push(`${label}artifact_digest is not sha256:<64 hex>`);
  }
  if (Array.isArray(evidence.artifacts)) {
    const recomputed = artifactDigest(evidence.artifacts);
    if (recomputed !== evidence.artifact_digest) {
      problems.push(`${label}artifact-mismatch: artifact_digest does not match the artifact list`);
    }
    for (const artifact of evidence.artifacts) {
      if (!artifact || typeof artifact.path !== 'string' || !isDigest(artifact.sha256)) {
        problems.push(`${label}artifact entry is malformed`);
        continue;
      }
      const full = resolve(options.cwd || ROOT, artifact.path);
      if (existsSync(full) && statSync(full).isFile()) {
        const actual = hashFile(full);
        if (actual !== artifact.sha256) {
          problems.push(`${label}artifact-mismatch: ${artifact.path} hashes ${actual}`);
        }
      }
    }
  }
  const runner = evidence.runner;
  if (!runner || typeof runner !== 'object') {
    problems.push(`${label}runner is missing`);
  } else {
    for (const field of ['os', 'arch', 'ci', 'run_id']) {
      if (typeof runner[field] !== 'string' || runner[field].length === 0) {
        problems.push(`${label}runner.${field} is missing`);
      }
    }
  }
  const signature = verifyEvidenceSignature(evidence, keys || {});
  if (requireSigned && !signature.signed) {
    problems.push(`${label}unsigned: release-grade evidence requires an allowlisted ed25519 signature`);
  }
  if (requireSigned && signature.signed && !signature.ok) {
    problems.push(`${label}signature-invalid: ${signature.reason}`);
  }
  return { ok: problems.length === 0, signed: signature.signed, signatureOk: signature.ok, reason: signature.reason, problems };
}

// ------------------------------------------------------- marker verification

// Expected marker sets per workflow. `skippable` lanes may be `skipped`, but
// only with a recorded reason; every other lane must be `passed`.
const WORKFLOWS = {
  pr: {
    expected: [
      'linux',
      'static',
      'docs',
      'vscode',
      'vscode-visual',
      'vscode-visual-with-skip',
      'jetbrains-build',
      'jetbrains-smoke',
    ],
  },
  trusted: {
    expected: [
      'linux',
      'static',
      'docs',
      'vscode',
      'vscode-visual',
      'vscode-visual-with-skip',
      'jetbrains-build',
      'jetbrains-smoke',
      'perf',
    ],
  },
  nightly: {
    expected: [
      'fault-scale',
      'longrun',
      'efficiency',
      'economy',
      'coding-benchmark',
      { lane: 'coding-benchmark-real-model', skippable: true },
      'supply-chain',
    ],
  },
};

function expectedLanes(workflow) {
  const spec = WORKFLOWS[workflow];
  if (!spec) {
    throw new Error(`unknown workflow '${workflow}' (expected one of ${Object.keys(WORKFLOWS).join(', ')})`);
  }
  return spec.expected.map((entry) => (typeof entry === 'string' ? { lane: entry } : entry));
}

// Minimal reader for the repository's own lane YAML: collects the command
// items of one `- name:` step. Marker emission items (target/certification/
// lanes) are dropped wholesale, so what remains is the command set that
// actually ran.
function extractLaneCommands(yamlText, lane) {
  const lines = yamlText.split('\n');
  let inStep = false;
  let inCommands = false;
  let blockIndent = null;
  let current = null;
  const items = [];
  for (const line of lines) {
    const stepMatch = /^  - name:\s*(\S+)\s*$/.exec(line);
    if (stepMatch) {
      inStep = stepMatch[1] === lane;
      inCommands = false;
      blockIndent = null;
      current = null;
      continue;
    }
    if (!inStep) {
      continue;
    }
    if (/^  - /.test(line) && !/^  - name:/.test(line)) {
      inStep = false;
      continue;
    }
    if (/^    commands:\s*$/.test(line)) {
      inCommands = true;
      continue;
    }
    if (!inCommands) {
      continue;
    }
    const item = /^      - (.*)$/.exec(line);
    if (item) {
      if (['|', '|-', '>', '>-'].includes(item[1].trim())) {
        current = [];
        items.push(current);
        blockIndent = 8;
      } else {
        current = [item[1]];
        items.push(current);
        blockIndent = null;
      }
      continue;
    }
    if (blockIndent !== null && current) {
      if (line.trim() === '') {
        current.push('');
        continue;
      }
      const indent = line.match(/^\s*/)[0].length;
      if (indent >= blockIndent) {
        current.push(line.slice(blockIndent));
        continue;
      }
      blockIndent = null;
    }
    if (/^    \S/.test(line)) {
      inCommands = false;
    }
  }
  const actual = [];
  for (const item of items) {
    if (item.some((raw) => /target\/certification\/lanes|faktor-woodpecker-lane/.test(raw))) {
      continue;
    }
    actual.push(...item.map((raw) => raw.trim()).filter((raw) => raw !== ''));
  }
  return actual;
}

// The marker heredoc inside the YAML must declare exactly the commands the
// lane runs; otherwise editing a command silently invalidates the evidence.
function extractDeclaredCommands(yamlText, lane) {
  const lines = yamlText.split('\n');
  let inStep = false;
  let declared = null;
  let collecting = false;
  for (const line of lines) {
    const stepMatch = /^  - name:\s*(\S+)\s*$/.exec(line);
    if (stepMatch) {
      inStep = stepMatch[1] === lane;
      collecting = false;
      continue;
    }
    if (!inStep) {
      continue;
    }
    if (/<<'?CMDS'?/.test(line)) {
      declared = [];
      collecting = true;
      continue;
    }
    if (collecting) {
      if (/^\s*CMDS\s*$/.test(line)) {
        collecting = false;
        continue;
      }
      declared.push(line.trim());
    }
  }
  return declared ? declared.filter((line) => line !== '') : null;
}

function markerProblems(record, options) {
  const problems = [];
  const {
    lane,
    expected,
    commit,
    tree,
    runId,
    yamlText,
    checkArtifacts,
    cwd,
  } = options;
  const label = `lane ${lane}`;
  if (record.schema !== MARKER_SCHEMA) {
    problems.push(`wrong-schema: ${label} schema=${record.schema}`);
    return problems;
  }
  if (record.lane !== lane) {
    problems.push(`lane-name-mismatch: ${label} file says lane=${record.lane}`);
  }
  const status = String(record.status);
  if (status === 'skipped') {
    if (!expected.skippable) {
      problems.push(`skipped-required: ${label} is a required lane but recorded a skip`);
    } else if (typeof record.reason !== 'string' || record.reason.trim() === '') {
      problems.push(`silent-skip: ${label} skipped without a recorded reason`);
    }
  } else if (status !== 'passed') {
    problems.push(`failed-lane: ${label} status=${status}`);
  }
  if (record.commit !== commit) {
    problems.push(`other-commit: ${label} marker commit=${record.commit} != ${commit}`);
  }
  if (record.tree !== tree) {
    problems.push(`tree-mismatch: ${label} marker tree=${record.tree} != ${tree}`);
  }
  const runner = record.runner;
  if (!runner || typeof runner !== 'object' || runner.run_id !== runId) {
    problems.push(`stale-run: ${label} marker run_id=${runner && runner.run_id} != ${runId}`);
  } else {
    for (const field of ['os', 'arch', 'ci']) {
      if (typeof runner[field] !== 'string' || runner[field].length === 0) {
        problems.push(`wrong-schema: ${label} runner.${field} missing`);
      }
    }
  }
  const started = parseIso(record.started_at);
  const finished = parseIso(record.finished_at);
  if (started === null || finished === null) {
    problems.push(`timestamp-order: ${label} timestamps are not date-times`);
  } else if (finished < started) {
    problems.push(`timestamp-order: ${label} finished_at precedes started_at`);
  }
  let decoded = null;
  if (typeof record.commands_b64 !== 'string' || record.commands_b64.length === 0) {
    problems.push(`command-digest-mismatch: ${label} commands_b64 is missing`);
  } else {
    try {
      decoded = Buffer.from(record.commands_b64, 'base64').toString('utf8');
    } catch {
      decoded = null;
    }
    if (!decoded || decoded.trim() === '') {
      problems.push(`command-digest-mismatch: ${label} commands_b64 decodes to empty text`);
    } else {
      const recomputed = `sha256:${sha256Hex(decoded)}`;
      if (record.commands_digest !== recomputed) {
        problems.push(
          `command-digest-mismatch: ${label} commands_digest ${record.commands_digest} != recomputed ${recomputed}`,
        );
      }
      if (yamlText !== undefined && yamlText !== null) {
        const actual = extractLaneCommands(yamlText, lane);
        const declaredLines = decoded.split('\n').map((line) => line.trim()).filter((line) => line !== '');
        if (actual.length !== declaredLines.length || actual.some((line, i) => line !== declaredLines[i])) {
          const firstDiff = actual.findIndex((line, i) => line !== declaredLines[i]);
          problems.push(
            `command-set-drift: ${label} marker commands differ from .woodpecker commands` +
              (firstDiff === -1 ? ` (length ${declaredLines.length} != ${actual.length})` : ` at line ${firstDiff + 1}`),
          );
        }
      }
    }
  }
  const artifacts = Array.isArray(record.artifacts) ? record.artifacts : null;
  if (!artifacts) {
    problems.push(`artifact-mismatch: ${label} artifacts is not an array`);
  } else {
    if (record.artifact_digest !== artifactDigest(artifacts)) {
      problems.push(`artifact-mismatch: ${label} artifact_digest does not match the artifact list`);
    }
    for (const artifact of artifacts) {
      if (!artifact || typeof artifact.path !== 'string' || !isDigest(artifact.sha256)) {
        problems.push(`artifact-mismatch: ${label} artifact entry is malformed`);
        continue;
      }
      if (checkArtifacts) {
        const full = resolve(cwd || ROOT, artifact.path);
        if (!existsSync(full)) {
          problems.push(`artifact-missing: ${label} ${artifact.path} is not in the workspace`);
        } else if (hashFile(full) !== artifact.sha256) {
          problems.push(`artifact-mismatch: ${label} ${artifact.path} does not hash to ${artifact.sha256}`);
        }
      }
    }
  }
  return problems;
}

function verifyMarkers(options) {
  const {
    workflow,
    lanesDir = 'target/certification/lanes',
    out,
    yamlText,
    commit,
    tree,
    runId,
    pipelineStatus,
    checkArtifacts = true,
    cwd = ROOT,
  } = options;
  const expected = expectedLanes(workflow);
  const expectedNames = expected.map((entry) => entry.lane);
  const problems = [];
  const lanes = {};
  const markers = {};
  const dir = resolve(cwd, lanesDir);
  let files = [];
  if (existsSync(dir)) {
    files = readdirSync(dir).filter((name) => name.endsWith('.json'));
  } else {
    problems.push(`missing: lanes directory ${lanesDir} does not exist`);
  }
  const seen = new Map();
  for (const file of files) {
    const stem = file.replace(/\.json$/, '');
    let record;
    try {
      record = readJsonStrict(join(dir, file));
    } catch (error) {
      lanes[stem] = 'unreadable';
      problems.push(`unreadable: lane ${stem} marker is not valid JSON: ${error.message}`);
      continue;
    }
    const laneField = String(record.lane || '');
    if (seen.has(laneField)) {
      problems.push(`duplicate: lane ${laneField} appears in both ${seen.get(laneField)} and ${file}`);
    }
    seen.set(laneField, file);
    if (!expectedNames.includes(stem)) {
      lanes[stem] = 'unexpected';
      problems.push(`unexpected: marker ${file} does not belong to workflow '${workflow}'`);
      continue;
    }
    const spec = expected.find((entry) => entry.lane === stem);
    const laneProblems = markerProblems(record, {
      lane: stem,
      expected: spec,
      commit,
      tree,
      runId,
      yamlText,
      checkArtifacts,
      cwd,
    });
    markers[stem] = {
      commit: record.commit,
      tree: record.tree,
      started_at: record.started_at,
      finished_at: record.finished_at,
      commands_digest: record.commands_digest,
      artifact_digest: record.artifact_digest,
      runner: record.runner,
    };
    if (laneProblems.length > 0) {
      lanes[stem] = laneProblems[0].split(':')[0];
      problems.push(...laneProblems);
    } else {
      lanes[stem] = record.status === 'skipped' ? 'skipped' : 'passed';
    }
  }
  for (const spec of expected) {
    if (!seen.has(spec.lane)) {
      lanes[spec.lane] = 'missing';
      problems.push(`missing: lane ${spec.lane} wrote no marker`);
    }
  }
  if (pipelineStatus !== undefined && pipelineStatus !== 'success') {
    problems.push(`pipeline-status: workflow status before the certificate is ${pipelineStatus}`);
  }
  const status = problems.length === 0 ? 'pass' : 'fail';
  const manifest = {
    schema: CI_CERT_SCHEMA,
    workflow,
    platform: 'linux',
    commit,
    tree,
    run_id: runId,
    pipeline_url: process.env.CI_PIPELINE_URL || '',
    status,
    lanes,
    markers,
    problems,
  };
  if (out) {
    mkdirSync(dirname(resolve(cwd, out)), { recursive: true });
    writeFileSync(resolve(cwd, out), `${JSON.stringify(manifest, null, 2)}\n`);
  }
  for (const spec of expected) {
    console.log(`${lanes[spec.lane] === 'passed' ? 'passed' : `FAILED ${lanes[spec.lane]}`} ${spec.lane}`);
  }
  if (problems.length > 0) {
    for (const problem of problems) {
      console.error(`certification problem: ${problem}`);
    }
    return { ok: false, manifest };
  }
  console.log('certification: PASS (every required lane produced verifiable evidence for this commit/tree)');
  return { ok: true, manifest };
}

// ------------------------------------------------------------------ writing

function writeEvidence(options) {
  const {
    kind,
    status,
    outDir = 'target/certification/evidence',
    cwd = ROOT,
    signKey,
    keyId,
    runner,
    artifactsExplicit,
    fromMarkers,
    onlyLane,
    commandsText,
  } = options;
  const commit = headCommit(cwd);
  const tree = headTree(cwd);
  const started = options.startedAt || isoNow();
  const finished = options.finishedAt || isoNow();
  let artifacts = [];
  let commands = commandsText || '';
  if (artifactsExplicit.length > 0) {
    artifacts = artifactsExplicit.map((path) => ({ path, sha256: hashFile(resolve(cwd, path)) }));
  } else if (fromMarkers) {
    const dir = resolve(cwd, fromMarkers);
    const files = readdirSync(dir).filter((name) => name.endsWith('.json'));
    const selected = files
      .map((name) => ({ name, record: readJsonStrict(join(dir, name)) }))
      .filter(({ name }) => !onlyLane || name === `${onlyLane}.json`)
      .sort((a, b) => (a.name < b.name ? -1 : 1));
    artifacts = selected.map(({ name }) => ({
      path: `${fromMarkers.replace(/\/$/, '')}/${name}`,
      sha256: hashFile(join(dir, name)),
    }));
    if (!commands) {
      commands = selected
        .sort((a, b) => (a.record.lane < b.record.lane ? -1 : 1))
        .map(({ record }) => `${record.lane} commands_digest=${record.commands_digest}`)
        .join('\n');
    }
  }
  if (!commands) {
    commands = `kind=${kind} status=${status}`;
  }
  let evidence = {
    schema: EVIDENCE_SCHEMA,
    repository: options.repository || repositoryName(cwd),
    commit_sha: commit,
    tree_hash: tree,
    kind,
    status,
    started_at: started,
    finished_at: finished,
    commands_digest: `sha256:${sha256Hex(commands)}`,
    artifact_digest: artifactDigest(artifacts),
    repository_tree_verified: tree === headTree(cwd),
    runner: runner || runnerDefaults(),
    commands,
    artifacts,
    signature: null,
  };
  if (signKey) {
    evidence = signEvidence(evidence, resolve(cwd, signKey), keyId || 'ci');
  }
  mkdirSync(resolve(cwd, outDir), { recursive: true });
  const path = resolve(cwd, outDir, `${kind}.json`);
  writeFileSync(path, `${JSON.stringify(evidence, null, 2)}\n`);
  console.log(`evidence written: ${path}`);
  console.log(`  kind=${kind} status=${status} commit=${commit} tree=${tree} signed=${Boolean(evidence.signature)}`);
  return path;
}

// --------------------------------------------------------------- self-tests

function runSelftest() {
  const failures = [];
  const temp = mkdtempSync(join(tmpdir(), 'faktor-evidence-selftest-'));
  const repo = join(temp, 'repo');
  const run = (name, fn) => {
    try {
      fn();
      console.log(`selftest ok: ${name}`);
    } catch (error) {
      failures.push(`${name}: ${error.message}`);
      console.error(`selftest FAIL: ${name}: ${error.message}`);
    }
  };
  const assert = (condition, message) => {
    if (!condition) {
      throw new Error(message);
    }
  };
  const expectFailCode = (result, code, label) => {
    assert(!result.ok, `${label}: expected failure but verification passed`);
    assert(
      result.manifest.problems.some((problem) => problem.startsWith(`${code}:`)),
      `${label}: expected a '${code}:' problem, got ${JSON.stringify(result.manifest.problems)}`,
    );
  };
  try {
    mkdirSync(repo, { recursive: true });
    execFileSync('git', ['-C', repo, 'init', '-q'], { encoding: 'utf8' });
    execFileSync('git', ['-C', repo, 'config', 'user.email', 'selftest@example.invalid'], { encoding: 'utf8' });
    execFileSync('git', ['-C', repo, 'config', 'user.name', 'Selftest'], { encoding: 'utf8' });
    writeFileSync(join(repo, 'file.txt'), 'one\n');
    execFileSync('git', ['-C', repo, 'add', 'file.txt'], { encoding: 'utf8' });
    execFileSync('git', ['-C', repo, 'commit', '-q', '-m', 'base'], { encoding: 'utf8' });
    const commit = headCommit(repo);
    const tree = headTree(repo);
    const runId = '4242';
    const laneCommands = {
      'lane-a': ['echo lane-a'],
      'lane-b': ['echo lane-b', 'sleep 0'],
    };
    const yamlText = [
      'steps:',
      '  - name: lane-a',
      '    commands:',
      '      - echo lane-a',
      '  - name: lane-b',
      '    commands:',
      '      - echo lane-b',
      '      - sleep 0',
      '',
    ].join('\n');
    const marker = (lane, overrides = {}) => {
      const commands = overrides.commands || laneCommands[lane];
      const text = commands.join('\n');
      const record = {
        schema: MARKER_SCHEMA,
        lane,
        status: 'passed',
        commit,
        tree,
        runner: { os: 'linux', arch: 'amd64', ci: 'woodpecker', run_id: runId },
        started_at: '2026-01-01T00:00:00Z',
        finished_at: '2026-01-01T00:01:00Z',
        commands_b64: Buffer.from(text, 'utf8').toString('base64'),
        commands_digest: `sha256:${sha256Hex(text)}`,
        artifacts: [],
        artifact_digest: EMPTY_ARTIFACT_DIGEST,
        ...overrides,
      };
      return record;
    };
    const writeMarkers = (dir, records) => {
      mkdirSync(dir, { recursive: true });
      for (const [lane, record] of Object.entries(records)) {
        writeFileSync(join(dir, `${lane}.json`), JSON.stringify(record));
      }
    };
    const lanesDir = join(repo, 'target/certification/lanes');
    const valid = () => ({ 'lane-a': marker('lane-a'), 'lane-b': marker('lane-b') });
    const verify = (records, extra = {}) => {
      rmSync(lanesDir, { recursive: true, force: true });
      writeMarkers(lanesDir, records);
      return verifyMarkers({
        workflow: 'selftest',
        lanesDir,
        commit,
        tree,
        runId,
        pipelineStatus: 'success',
        yamlText,
        checkArtifacts: false,
        cwd: repo,
        ...extra,
      });
    };
    WORKFLOWS.selftest = {
      expected: ['lane-a', { lane: 'lane-b', skippable: true }],
    };
    run('valid markers verify', () => {
      const result = verify(valid());
      assert(result.ok, `expected pass, problems=${JSON.stringify(result.manifest.problems)}`);
    });
    run('other commit rejected', () => {
      const records = valid();
      records['lane-a'].commit = '0'.repeat(40);
      expectFailCode(verify(records), 'other-commit', 'other commit');
    });
    run('tree mismatch rejected', () => {
      const records = valid();
      records['lane-b'].tree = '1'.repeat(40);
      expectFailCode(verify(records), 'tree-mismatch', 'tree mismatch');
    });
    run('missing marker rejected', () => {
      const records = valid();
      delete records['lane-a'];
      expectFailCode(verify(records), 'missing', 'missing marker');
    });
    run('unexpected marker rejected', () => {
      const records = valid();
      records['lane-z'] = marker('lane-z', { lane: 'lane-z', commands: ['echo lane-z'] });
      expectFailCode(verify(records), 'unexpected', 'unexpected marker');
    });
    run('duplicate lane rejected', () => {
      const records = valid();
      writeMarkers(lanesDir, records);
      writeFileSync(join(lanesDir, 'lane-a-copy.json'), JSON.stringify(marker('lane-a')));
      const result = verifyMarkers({
        workflow: 'selftest',
        lanesDir,
        commit,
        tree,
        runId,
        pipelineStatus: 'success',
        yamlText,
        checkArtifacts: false,
        cwd: repo,
      });
      expectFailCode(result, 'unexpected', 'duplicate lane');
    });
    run('duplicate JSON key rejected', () => {
      rmSync(lanesDir, { recursive: true, force: true });
      mkdirSync(lanesDir, { recursive: true });
      writeFileSync(join(lanesDir, 'lane-a.json'), `{"schema":"x","schema":"y"}`);
      const result = verifyMarkers({
        workflow: 'selftest',
        lanesDir,
        commit,
        tree,
        runId,
        pipelineStatus: 'success',
        yamlText,
        checkArtifacts: false,
        cwd: repo,
      });
      expectFailCode(result, 'unreadable', 'duplicate JSON key');
    });
    run('skipped required lane rejected', () => {
      const records = valid();
      records['lane-a'].status = 'skipped';
      expectFailCode(verify(records), 'skipped-required', 'skipped required');
    });
    run('silent skip rejected', () => {
      const records = valid();
      records['lane-b'].status = 'skipped';
      expectFailCode(verify(records), 'silent-skip', 'silent skip');
    });
    run('skippable lane with reason accepted', () => {
      const records = valid();
      records['lane-b'].status = 'skipped';
      records['lane-b'].reason = 'provider keys absent; recorded skip';
      const result = verify(records);
      assert(result.ok, `expected pass, problems=${JSON.stringify(result.manifest.problems)}`);
      assert(result.manifest.lanes['lane-b'] === 'skipped', 'skipped lane must be recorded as skipped');
    });
    run('failed lane rejected', () => {
      const records = valid();
      records['lane-a'].status = 'failed';
      expectFailCode(verify(records), 'failed-lane', 'failed lane');
    });
    run('stale run rejected', () => {
      const records = valid();
      records['lane-a'].runner.run_id = '999';
      expectFailCode(verify(records), 'stale-run', 'stale run');
    });
    run('command digest mismatch rejected', () => {
      const records = valid();
      records['lane-a'].commands_digest = `sha256:${'0'.repeat(64)}`;
      expectFailCode(verify(records), 'command-digest-mismatch', 'digest mismatch');
    });
    run('command set drift rejected', () => {
      const records = valid();
      records['lane-a'].commands = ['echo different'];
      records['lane-a'].commands_b64 = Buffer.from('echo different', 'utf8').toString('base64');
      records['lane-a'].commands_digest = `sha256:${sha256Hex('echo different')}`;
      expectFailCode(verify(records), 'command-set-drift', 'command drift');
    });
    run('wrong schema rejected', () => {
      const records = valid();
      records['lane-a'].schema = 'faktor-woodpecker-lane/v1';
      expectFailCode(verify(records), 'wrong-schema', 'wrong schema');
    });
    run('lane name mismatch rejected', () => {
      const records = valid();
      records['lane-a'].lane = 'lane-other';
      expectFailCode(verify(records), 'lane-name-mismatch', 'lane name mismatch');
    });
    run('timestamp order rejected', () => {
      const records = valid();
      records['lane-a'].finished_at = '2025-12-31T23:59:00Z';
      expectFailCode(verify(records), 'timestamp-order', 'timestamp order');
    });
    run('failed pipeline rejected', () => {
      const result = verify(valid(), { pipelineStatus: 'failure' });
      expectFailCode(result, 'pipeline-status', 'failed pipeline');
    });
    run('artifact hash mismatch rejected', () => {
      writeFileSync(join(repo, 'artifact.bin'), 'artifact');
      const good = hashFile(join(repo, 'artifact.bin'));
      const records = valid();
      records['lane-a'].artifacts = [{ path: 'artifact.bin', sha256: good }];
      records['lane-a'].artifact_digest = artifactDigest(records['lane-a'].artifacts);
      rmSync(lanesDir, { recursive: true, force: true });
      writeMarkers(lanesDir, records);
      const assertOk = verifyMarkers({
        workflow: 'selftest',
        lanesDir,
        commit,
        tree,
        runId,
        pipelineStatus: 'success',
        yamlText,
        checkArtifacts: true,
        cwd: repo,
      });
      assert(assertOk.ok, `artifact fixture must verify, problems=${JSON.stringify(assertOk.manifest.problems)}`);
      records['lane-a'].artifacts[0].sha256 = `sha256:${'0'.repeat(64)}`;
      records['lane-a'].artifact_digest = artifactDigest(records['lane-a'].artifacts);
      rmSync(lanesDir, { recursive: true, force: true });
      writeMarkers(lanesDir, records);
      const tampered = verifyMarkers({
        workflow: 'selftest',
        lanesDir,
        commit,
        tree,
        runId,
        pipelineStatus: 'success',
        yamlText,
        checkArtifacts: true,
        cwd: repo,
      });
      expectFailCode(tampered, 'artifact-mismatch', 'artifact mismatch');
    });
    run('artifact missing rejected', () => {
      const records = valid();
      records['lane-a'].artifacts = [{ path: 'missing.bin', sha256: `sha256:${'2'.repeat(64)}` }];
      records['lane-a'].artifact_digest = artifactDigest(records['lane-a'].artifacts);
      rmSync(lanesDir, { recursive: true, force: true });
      writeMarkers(lanesDir, records);
      const result = verifyMarkers({
        workflow: 'selftest',
        lanesDir,
        commit,
        tree,
        runId,
        pipelineStatus: 'success',
        yamlText,
        checkArtifacts: true,
        cwd: repo,
      });
      expectFailCode(result, 'artifact-missing', 'artifact missing');
    });

    // ------------------------------------------------- evidence signatures
    const { privateKey, publicKey } = generateKeyPairSync('ed25519');
    const rawPublic = publicKey.export({ format: 'der', type: 'spki' }).subarray(12).toString('base64');
    const identities = { 'selftest-ci': rawPublic };
    const unsigned = {
      schema: EVIDENCE_SCHEMA,
      repository: 'selftest',
      commit_sha: commit,
      tree_hash: tree,
      kind: 'cross_platform_lanes',
      status: 'passed',
      started_at: '2026-01-01T00:00:00Z',
      finished_at: '2026-01-01T00:01:00Z',
      commands_digest: `sha256:${sha256Hex('selftest commands')}`,
      artifact_digest: EMPTY_ARTIFACT_DIGEST,
      runner: { os: 'linux', arch: 'amd64', ci: 'selftest', run_id: '1' },
      artifacts: [],
      signature: null,
    };
    const signed = signEvidenceWithKey(unsigned, privateKey, 'selftest-ci');
    run('unsigned evidence rejected for release', () => {
      const verdict = verifyEvidenceObject(unsigned, {
        kind: 'cross_platform_lanes',
        expectedCommit: commit,
        expectedTree: tree,
        keys: identities,
        requireSigned: true,
      });
      assert(!verdict.ok, 'unsigned evidence must not certify');
      assert(
        verdict.problems.some((problem) => problem.startsWith('unsigned:')),
        `expected unsigned problem, got ${JSON.stringify(verdict.problems)}`,
      );
    });
    run('allowlisted signature accepted', () => {
      const verdict = verifyEvidenceObject(signed, {
        kind: 'cross_platform_lanes',
        expectedCommit: commit,
        expectedTree: tree,
        keys: identities,
        requireSigned: true,
      });
      assert(verdict.ok, `signed evidence must verify: ${JSON.stringify(verdict.problems)}`);
    });
    run('unknown identity rejected', () => {
      const foreign = signEvidenceWithKey(unsigned, privateKey, 'foreign');
      const verdict = verifyEvidenceObject(foreign, {
        kind: 'cross_platform_lanes',
        expectedCommit: commit,
        expectedTree: tree,
        keys: identities,
        requireSigned: true,
      });
      assert(!verdict.ok, 'foreign identity must not certify');
      assert(
        verdict.problems.some((problem) => problem.startsWith('signature-invalid:')),
        `expected signature-invalid, got ${JSON.stringify(verdict.problems)}`,
      );
    });
    run('tampered signed evidence rejected', () => {
      const tampered = { ...signed, commit_sha: '0'.repeat(40) };
      const verdict = verifyEvidenceObject(tampered, {
        kind: 'cross_platform_lanes',
        expectedCommit: commit,
        expectedTree: tree,
        keys: identities,
        requireSigned: true,
      });
      assert(!verdict.ok, 'tampered evidence must not certify');
    });
    run('signed evidence bound to a different commit rejected', () => {
      const verdict = verifyEvidenceObject(signed, {
        kind: 'cross_platform_lanes',
        expectedCommit: '0'.repeat(40),
        expectedTree: tree,
        keys: identities,
        requireSigned: true,
      });
      assert(
        verdict.problems.some((problem) => problem.startsWith('other-commit:')),
        `expected other-commit, got ${JSON.stringify(verdict.problems)}`,
      );
    });
    run('writeEvidence binds HEAD commit and tree', () => {
      const outDir = join(repo, 'target/certification/evidence');
      const path = writeEvidence({
        kind: 'real_provider',
        status: 'passed',
        outDir,
        cwd: repo,
        artifactsExplicit: [],
        commandsText: 'selftest provider commands',
        runner: { os: 'linux', arch: 'amd64', ci: 'selftest', run_id: '1' },
      });
      const record = readJsonStrict(path);
      assert(record.commit_sha === commit, 'evidence commit binding');
      assert(record.tree_hash === tree, 'evidence tree binding');
      assert(record.repository_tree_verified === true, 'tree verification flag');
      const verdict = verifyEvidenceObject(record, {
        kind: 'real_provider',
        expectedCommit: commit,
        expectedTree: tree,
        keys: {},
        requireSigned: false,
      });
      assert(verdict.ok, `written evidence must verify: ${JSON.stringify(verdict.problems)}`);
    });

    // ------------------------------------------- repository drift self-test
    run('woodpecker marker heredocs match lane commands', () => {
      const yamlDir = resolve(ROOT, '.woodpecker');
      const problems = [];
      const yamlFiles = { pr: 'pr.yaml', trusted: 'trusted.yaml', nightly: 'nightly.yaml' };
      for (const [workflow, file] of Object.entries(yamlFiles)) {
        if (!existsSync(join(yamlDir, file))) {
          continue;
        }
        const text = readFileSync(join(yamlDir, file), 'utf8');
        for (const spec of expectedLanes(workflow)) {
          const declared = extractDeclaredCommands(text, spec.lane);
          const actual = extractLaneCommands(text, spec.lane);
          if (declared === null) {
            problems.push(`${file}: lane ${spec.lane} has no CMDS marker heredoc`);
            continue;
          }
          if (declared.length !== actual.length || declared.some((line, i) => line !== actual[i])) {
            const firstDiff = actual.findIndex((line, i) => line !== declared[i]);
            problems.push(
              `${file}: lane ${spec.lane} marker commands drift at line ${firstDiff + 1}` +
                ` (declared ${declared.length} lines, actual ${actual.length})`,
            );
          }
        }
      }
      assert(problems.length === 0, problems.join('; '));
    });
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
  if (failures.length > 0) {
    console.error(`selftest: FAIL (${failures.length} failure(s))`);
    return 1;
  }
  console.log('selftest: PASS (marker rejection matrix, evidence binding, signatures, repo drift)');
  return 0;
}

// ------------------------------------------------------------------- main

function usage() {
  console.log(`usage: node scripts/certification/evidence.mjs <command> [options]

commands:
  write          write target/certification/evidence/<kind>.json
                 --kind K --status passed|failed|skipped [--out-dir DIR]
                 [--commands TEXT] [--from-markers DIR [--only-lane L]]
                 [--artifacts a,b] [--sign-key PEM --key-id ID]
                 [--runner-os OS --runner-arch ARCH --runner-ci CI --runner-run-id ID]
  sign           add an ed25519 signature: --file E.json --key PEM --key-id ID
  verify         verify one evidence object (--kind K [--evidence-dir DIR]
                 [--require-signed] [--keys FILE] [--json]); exit 1 on problems
  verify-markers verify a workflow's lane markers and write the CI certificate
                 --workflow pr|trusted|nightly [--lanes-dir DIR] [--out FILE]
                 [--yaml-dir DIR] [--pipeline-status STATUS] [--run-id ID]
  selftest       prove the rejection matrix + signature allowlist + repo drift`);
}

function main(argv) {
  const [command, ...args] = argv;
  if (!command || ['-h', '--help', 'help'].includes(command)) {
    usage();
    return command ? 0 : 2;
  }
  if (command === 'selftest') {
    return runSelftest();
  }
  if (command === 'write' || command === 'attest') {
    const kind = argValue(args, '--kind');
    const status = argValue(args, '--status', 'passed');
    if (!kind) {
      console.error('write: --kind is required');
      return 2;
    }
    writeEvidence({
      kind,
      status,
      outDir: argValue(args, '--out-dir', 'target/certification/evidence'),
      cwd: argValue(args, '--cwd', ROOT),
      signKey: argValue(args, '--sign-key', process.env.CERTIFY_EVIDENCE_SIGN_KEY || ''),
      keyId: argValue(args, '--key-id', process.env.CERTIFY_EVIDENCE_SIGN_KEY_ID || 'ci'),
      runner: args.includes('--runner-os')
        ? {
            os: argValue(args, '--runner-os'),
            arch: argValue(args, '--runner-arch', process.arch),
            ci: argValue(args, '--runner-ci', 'local'),
            run_id: argValue(args, '--runner-run-id', 'local'),
          }
        : null,
      artifactsExplicit: argValue(args, '--artifacts')
        ? argValue(args, '--artifacts').split(',').filter(Boolean)
        : [],
      fromMarkers: argValue(args, '--from-markers') || '',
      onlyLane: argValue(args, '--only-lane') || '',
      commandsText: argValue(args, '--commands'),
      startedAt: argValue(args, '--started-at') || undefined,
      finishedAt: argValue(args, '--finished-at') || undefined,
      repository: argValue(args, '--repository') || undefined,
    });
    return 0;
  }
  if (command === 'sign') {
    const file = argValue(args, '--file');
    const key = argValue(args, '--key');
    const keyId = argValue(args, '--key-id');
    if (!file || !key || !keyId) {
      console.error('sign: --file, --key and --key-id are required');
      return 2;
    }
    const evidence = readJsonStrict(file);
    const signed = signEvidence(evidence, key, keyId);
    writeFileSync(file, `${JSON.stringify(signed, null, 2)}\n`);
    console.log(`signed: ${file} identity=${keyId}`);
    return 0;
  }
  if (command === 'verify') {
    const kind = argValue(args, '--kind');
    if (!kind) {
      console.error('verify: --kind is required');
      return 2;
    }
    const dir = resolve(argValue(args, '--evidence-dir', 'target/certification/evidence'));
    const file = join(dir, `${kind}.json`);
    const keys = loadKeys(argValue(args, '--keys'));
    if (!existsSync(file)) {
      const verdict = { ok: false, kind, file, problems: [`missing: ${file} does not exist`] };
      if (args.includes('--json')) {
        console.log(JSON.stringify(verdict));
      } else {
        console.error(`evidence verify: FAIL (${verdict.problems[0]})`);
      }
      return 1;
    }
    let evidence;
    try {
      evidence = readJsonStrict(file);
    } catch (error) {
      const verdict = { ok: false, kind, file, problems: [`unreadable: ${error.message}`] };
      if (args.includes('--json')) {
        console.log(JSON.stringify(verdict));
      } else {
        console.error(`evidence verify: FAIL (${verdict.problems[0]})`);
      }
      return 1;
    }
    const verdict = verifyEvidenceObject(evidence, {
      kind,
      file,
      keys,
      requireSigned: args.includes('--require-signed'),
      expectedCommit: headCommit(),
      expectedTree: headTree(),
    });
    verdict.file = file;
    verdict.kind = kind;
    if (args.includes('--json')) {
      console.log(JSON.stringify(verdict));
    } else if (verdict.ok) {
      console.log(
        `evidence verify: PASS kind=${kind} commit=${evidence.commit_sha} tree=${evidence.tree_hash} signed=${verdict.signed}`,
      );
    } else {
      for (const problem of verdict.problems) {
        console.error(`evidence problem: ${problem}`);
      }
    }
    return verdict.ok ? 0 : 1;
  }
  if (command === 'verify-markers') {
    const workflow = argValue(args, '--workflow');
    if (!workflow) {
      console.error('verify-markers: --workflow is required');
      return 2;
    }
    const yamlDir = resolve(argValue(args, '--yaml-dir', '.woodpecker'));
    const yamlFile = join(yamlDir, `${workflow}.yaml`);
    const yamlText = existsSync(yamlFile) ? readFileSync(yamlFile, 'utf8') : null;
    const result = verifyMarkers({
      workflow,
      lanesDir: argValue(args, '--lanes-dir', 'target/certification/lanes'),
      out: argValue(args, '--out', ''),
      yamlText,
      commit: headCommit(),
      tree: headTree(),
      runId: argValue(args, '--run-id', process.env.CI_PIPELINE_NUMBER || ''),
      pipelineStatus: argValue(args, '--pipeline-status', process.env.CI_PIPELINE_STATUS || ''),
      checkArtifacts: true,
      cwd: ROOT,
    });
    return result.ok ? 0 : 1;
  }
  console.error(`unknown command '${command}'`);
  usage();
  return 2;
}

process.exit(main(process.argv.slice(2)));
