#!/usr/bin/env node
// Faktor trusted-build attestation (audit P1-J).
//
// A `faktor-build-attestation/v1` object binds ONE trusted Woodpecker
// pipeline to the exact source it built and to the bytes that pipeline
// produced. The trusted workflow's `attestation` step writes it and prints a
// base64 marker block into the step log; scripts/certify.sh later FETCHES
// that block from the Woodpecker API for the exact trusted pipeline,
// verifies the ed25519 signature with the allowlisted key (the embedded
// public key must equal it), checks repository/source/tree/workflow/event/
// pipeline-number/observed-pipeline-id binding, and re-hashes every
// local/shipped artifact against the attested digests. A release
// certificate is impossible without an attestation that verifies.
//
// Commands:
//   keygen  --out-key PRIVATE.pem --out-keys KEYS.json --key-id ID
//   create  --out FILE --workflow trusted --event push|tag \
//           --repo owner/name --source-sha SHA --tree-sha SHA \
//           --pipeline-number N [--pipeline-id ID] [--pipeline-url URL] \
//           (--ci-image-ref IMAGE | --workflow-file FILE --step-name NAME) \
//           [--rust-toolchain V | --rust-toolchain-file FILE] \
//           [--artifact PATH]... [--optional-artifact PATH]... \
//           [--from-lanes DIR] [--emit-log-block] \
//           (--sign-key PEM | --sign-key-env VAR)
//   verify  --attestation FILE --source-sha SHA --tree-sha SHA \
//           --workflow WORKFLOW [--repo OWNER/NAME] --keys KEYS.json \
//           [--require-signed] [--event EVENT] [--pipeline-number N] \
//           [--pipeline-id ID] [--artifact PATH]...
//   selftest
//
// Signature model: identical to scripts/certification/evidence.mjs — the
// signed payload is the canonical JSON (object keys sorted recursively) of
// the attestation WITHOUT its `signature` field; `keys.json` is
// {"identities":{"<identity>":{"ed25519_public_key":"<base64 raw 32B>"}}}.
//
// FAIL-CLOSED SIGNING: `create` NEVER writes an unsigned attestation. When
// neither --sign-key nor the --sign-key-env variable (default
// FAKTOR_ATTEST_SIGN_KEY_PEM) holds a key it exits 3 with the typed
// `signing-key-missing` error and leaves no artifact behind. `verify` still
// understands unsigned historical/foreign objects and refuses them with
// --require-signed (certify.sh uses that). The documented alternative path
// is Sigstore/keyless signing of the same `faktor-build-attestation/v1`
// payload: the workflow attests through cosign with its OIDC identity and
// the operator verifies the bundle against that identity instead of the
// ed25519 allowlist (see docs/certification.md §2.12).

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
  writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const SCHEMA = 'faktor-build-attestation/v1';
const MARKER_BEGIN = '-----BEGIN FAKTOR ATTESTATION-----';
const MARKER_END = '-----END FAKTOR ATTESTATION-----';
const ED25519_SPKI_PREFIX = '302a300506032b6570032100';

// Typed refusal codes surfaced on stderr as
// `attestation: <code>: <message>`; `signing-key-missing` additionally exits 3.
const ERROR_CODES = {
  SIGNING_KEY_MISSING: 'signing-key-missing',
};

class AttestationError extends Error {
  constructor(code, message) {
    super(message);
    this.name = 'AttestationError';
    this.code = code;
  }
}

// Workflow registry: which events may produce an attestation for a workflow.
const WORKFLOW_EVENTS = {
  trusted: ['push', 'tag'],
};

// ------------------------------------------------------------------ utils

function sha256Hex(text) {
  return createHash('sha256').update(text).digest('hex');
}

function hashFile(path) {
  return `sha256:${sha256Hex(readFileSync(path))}`;
}

function isoNow() {
  return new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
}

function isSha(value) {
  return typeof value === 'string' && /^[0-9a-f]{40}$/.test(value);
}

function isDigest(value) {
  return typeof value === 'string' && /^sha256:[0-9a-f]{64}$/.test(value);
}

function canonicalJson(value) {
  if (Array.isArray(value)) {
    return `[${value.map((item) => canonicalJson(item)).join(',')}]`;
  }
  if (value && typeof value === 'object') {
    const keys = Object.keys(value).sort();
    return `{${keys.map((key) => `${JSON.stringify(key)}:${canonicalJson(value[key])}`).join(',')}}`;
  }
  return JSON.stringify(value);
}

function argValue(args, name, fallback = '') {
  const index = args.indexOf(name);
  if (index === -1) {
    return fallback;
  }
  if (index + 1 >= args.length) {
    throw new Error(`${name} needs a value`);
  }
  return args[index + 1];
}

function argList(args, name) {
  const values = [];
  for (let i = 0; i < args.length; i += 1) {
    if (args[i] === name) {
      if (i + 1 >= args.length) {
        throw new Error(`${name} needs a value`);
      }
      values.push(args[i + 1]);
      i += 1;
    }
  }
  return values;
}

function git(args, cwd = ROOT) {
  return execFileSync('git', ['-C', cwd, ...args], { encoding: 'utf8' }).trim();
}

function readJsonStrict(path) {
  const text = readFileSync(path, 'utf8');
  const value = JSON.parse(text);
  if (!value || typeof value !== 'object' || Array.isArray(value)) {
    throw new Error(`${path} is not a JSON object`);
  }
  return value;
}

function publicKeyFromRawBase64(base64) {
  const raw = Buffer.from(base64, 'base64');
  if (raw.length !== 32) {
    throw new Error(`ed25519 public key must be 32 raw bytes (got ${raw.length})`);
  }
  return createPublicKey({
    key: Buffer.from(ED25519_SPKI_PREFIX + raw.toString('hex'), 'hex'),
    format: 'der',
    type: 'spki',
  });
}

function loadKeys(keysPath) {
  if (!keysPath) {
    throw new Error('key allowlist is required (--keys or FAKTOR_ATTEST_KEYS)');
  }
  const document = readJsonStrict(keysPath);
  const identities = {};
  if (document.identities && typeof document.identities === 'object') {
    for (const [identity, entry] of Object.entries(document.identities)) {
      identities[identity] = entry.ed25519_public_key || entry.public_key || '';
    }
  } else {
    for (const [identity, entry] of Object.entries(document)) {
      identities[identity] = typeof entry === 'string' ? entry : entry.ed25519_public_key || '';
    }
  }
  if (Object.keys(identities).length === 0) {
    throw new Error(`${keysPath} carries no identities`);
  }
  return identities;
}

function signaturePayload(attestation) {
  const { signature, ...rest } = attestation;
  void signature;
  return canonicalJson(rest);
}

function signAttestation(attestation, privateKey, identity) {
  const rawPublic = createPublicKey(privateKey).export({ format: 'der', type: 'spki' }).subarray(-32);
  const value = cryptoSign(null, Buffer.from(signaturePayload(attestation)), privateKey);
  return {
    ...attestation,
    signature: {
      algorithm: 'ed25519',
      identity,
      public_key: rawPublic.toString('base64'),
      value: value.toString('base64'),
    },
  };
}

// ------------------------------------------------------- workflow reading

// Minimal scan of the repository's own workflow file: the `image:` line of
// the step named `stepName` (steps are `  - name:` entries, so the image is
// the first `    image:` line after the matching name and before the next
// step).
function findStepImage(yamlText, stepName) {
  const lines = yamlText.split('\n');
  let inStep = false;
  for (const line of lines) {
    const stepMatch = /^  - name:\s*(\S+)\s*$/.exec(line);
    if (stepMatch) {
      inStep = stepMatch[1] === stepName;
      continue;
    }
    if (!inStep) {
      continue;
    }
    const image = /^    image:\s*(.+?)\s*$/.exec(line);
    if (image) {
      return image[1].split('#')[0].trim();
    }
  }
  return '';
}

function resolveCiImage(options) {
  const ref = options.ciImageRef || findStepImage(readFileSync(options.workflowFile, 'utf8'), options.stepName);
  if (!ref) {
    throw new Error('no CI image reference resolved (--ci-image-ref or --workflow-file/--step-name)');
  }
  const match = /@sha256:([0-9a-f]{64})$/.exec(ref);
  if (!match) {
    throw new Error(`CI image '${ref}' is not digest-pinned (@sha256:<64 hex>)`);
  }
  return { ref, digest: `sha256:${match[1]}` };
}

function resolveRustToolchain(options) {
  if (options.rustToolchain) {
    return options.rustToolchain;
  }
  const file = options.rustToolchainFile || 'rust-toolchain.toml';
  if (!existsSync(resolve(ROOT, file))) {
    throw new Error(`cannot read rust toolchain from ${file}`);
  }
  const text = readFileSync(resolve(ROOT, file), 'utf8');
  const match = /^\s*channel\s*=\s*"([^"]+)"/m.exec(text);
  if (!match) {
    throw new Error(`${file} has no channel pin`);
  }
  return match[1];
}

// ------------------------------------------------------------- artifacts

function addArtifact(map, path, cwd) {
  const full = resolve(cwd, path);
  if (!existsSync(full)) {
    throw new Error(`artifact '${path}' does not exist`);
  }
  const name = basename(path);
  if (map.has(name)) {
    throw new Error(`artifact name '${name}' is ambiguous (two artifacts share the basename)`);
  }
  map.set(name, { name, path, sha256: hashFile(full) });
}

function collectArtifacts(options) {
  const cwd = options.cwd || ROOT;
  const map = new Map();
  for (const path of options.artifacts || []) {
    addArtifact(map, path, cwd);
  }
  for (const path of options.optionalArtifacts || []) {
    if (existsSync(resolve(cwd, path))) {
      addArtifact(map, path, cwd);
    }
  }
  if (options.fromLanes) {
    const dir = resolve(cwd, options.fromLanes);
    if (!existsSync(dir)) {
      throw new Error(`--from-lanes ${options.fromLanes} does not exist`);
    }
    const files = readdirSync(dir).filter((name) => name.endsWith('.json')).sort();
    for (const name of files) {
      const record = readJsonStrict(join(dir, name));
      for (const artifact of Array.isArray(record.artifacts) ? record.artifacts : []) {
        if (!artifact || typeof artifact.path !== 'string' || !isDigest(artifact.sha256)) {
          throw new Error(`${name}: malformed lane artifact entry`);
        }
        const full = resolve(cwd, artifact.path);
        if (!existsSync(full)) {
          throw new Error(`${name}: attested artifact '${artifact.path}' is not in the workspace`);
        }
        const local = hashFile(full);
        if (local !== artifact.sha256) {
          throw new Error(`${name}: artifact '${artifact.path}' hashes ${local}, marker says ${artifact.sha256}`);
        }
        const artifactName = basename(artifact.path);
        if (map.has(artifactName)) {
          continue; // same artifact reported by two lanes; digest already checked
        }
        map.set(artifactName, { name: artifactName, path: artifact.path, sha256: local });
      }
    }
  }
  return [...map.values()].sort((a, b) => (a.name < b.name ? -1 : 1));
}

// ---------------------------------------------------------------- create

// Fail closed: there is no unsigned code path. A missing (or unreadable)
// signing key is a typed refusal and no attestation file is written, so a
// pipeline without the signing secret cannot publish non-release-grade bytes.
function resolveSigningKey(options) {
  if (options.signKey) {
    try {
      return { key: createPrivateKey(readFileSync(options.signKey)), source: `--sign-key ${options.signKey}` };
    } catch (error) {
      throw new AttestationError(
        ERROR_CODES.SIGNING_KEY_MISSING,
        `--sign-key ${options.signKey} is not a readable ed25519 private key: ${error.message}`,
      );
    }
  }
  const envName = options.signKeyEnv || 'FAKTOR_ATTEST_SIGN_KEY_PEM';
  if (process.env[envName]) {
    try {
      return { key: createPrivateKey(process.env[envName]), source: `$${envName}` };
    } catch (error) {
      throw new AttestationError(
        ERROR_CODES.SIGNING_KEY_MISSING,
        `$${envName} is not a valid ed25519 private key PEM: ${error.message}`,
      );
    }
  }
  throw new AttestationError(
    ERROR_CODES.SIGNING_KEY_MISSING,
    'refusing to write an unsigned attestation: no signing key present. ' +
      `Set the ${envName} secret (Woodpecker: environment: ${envName}: { from_secret: faktor_attest_signing_key }) ` +
      'or pass --sign-key; generate a keypair with `node scripts/certification/attestation.mjs keygen`. ' +
      'The documented alternative is Sigstore/keyless signing of the same payload ' +
      '(docs/certification.md §2.12); certify.sh refuses unsigned attestations, so no artifact was emitted.',
  );
}

function createAttestation(options) {
  const signing = resolveSigningKey(options);
  const {
    workflow,
    event,
    repo,
    sourceSha,
    treeSha,
    pipelineNumber,
    pipelineId,
    pipelineUrl,
    cwd = ROOT,
  } = options;
  const events = WORKFLOW_EVENTS[workflow];
  if (!events) {
    throw new Error(`unknown workflow '${workflow}' (registry: ${Object.keys(WORKFLOW_EVENTS).join(', ')})`);
  }
  if (!events.includes(event)) {
    throw new Error(`workflow '${workflow}' attests events ${events.join('|')}, not '${event}'`);
  }
  if (!repo || !repo.includes('/')) {
    throw new Error(`--repo must be owner/name (got '${repo}')`);
  }
  if (!isSha(sourceSha) || !isSha(treeSha)) {
    throw new Error('--source-sha and --tree-sha must be 40-hex commit/tree ids');
  }
  if (!pipelineNumber) {
    throw new Error('--pipeline-number is required');
  }
  const image = resolveCiImage(options);
  const artifacts = collectArtifacts(options);
  const artifactMap = {};
  for (const artifact of artifacts) {
    artifactMap[artifact.name] = artifact.sha256;
  }
  let repositoryTreeVerified = false;
  try {
    repositoryTreeVerified = git(['rev-parse', 'HEAD'], cwd) === sourceSha && git(['rev-parse', 'HEAD^{tree}'], cwd) === treeSha;
  } catch {
    repositoryTreeVerified = false;
  }
  let attestation = {
    schema: SCHEMA,
    repository: repo,
    source_sha: sourceSha,
    tree_sha: treeSha,
    workflow,
    event,
    pipeline_id: String(pipelineId || pipelineNumber),
    pipeline_number: String(pipelineNumber),
    pipeline_url: pipelineUrl || '',
    build_environment_image: image.ref,
    build_environment_digest: image.digest,
    rust_toolchain: resolveRustToolchain(options),
    repository_tree_verified: repositoryTreeVerified,
    artifacts: artifactMap,
    created_at: isoNow(),
    signature: null,
  };
  attestation = signAttestation(attestation, signing.key, options.keyId || 'faktor-ci');
  mkdirSync(resolve(dirname(options.out)), { recursive: true });
  writeFileSync(options.out, `${JSON.stringify(attestation, null, 2)}\n`);
  console.log(`attestation written: ${options.out}`);
  console.log(
    `  workflow=${workflow} event=${event} source=${sourceSha} tree=${treeSha} pipeline=${pipelineNumber} ` +
      `artifacts=${Object.keys(artifactMap).length} signed=true signer=${signing.source}`,
  );
  if (options.emitLogBlock) {
    const encoded = readFileSync(options.out).toString('base64');
    console.log(MARKER_BEGIN);
    console.log(encoded);
    console.log(MARKER_END);
  }
  return attestation;
}

// ---------------------------------------------------------------- verify

function verifyAttestation(attestation, options) {
  const problems = [];
  const label = options.label || 'attestation';
  if (attestation.schema !== SCHEMA) {
    problems.push(`${label}:schema: unexpected schema '${attestation.schema}'`);
    return problems;
  }
  if (options.sourceSha && attestation.source_sha !== options.sourceSha) {
    problems.push(`${label}:source-sha-mismatch: ${attestation.source_sha} != ${options.sourceSha}`);
  }
  if (options.treeSha && attestation.tree_sha !== options.treeSha) {
    problems.push(`${label}:tree-sha-mismatch: ${attestation.tree_sha} != ${options.treeSha}`);
  }
  if (options.repo && attestation.repository !== options.repo) {
    problems.push(`${label}:repository-mismatch: ${attestation.repository} != ${options.repo}`);
  }
  if (options.workflow && attestation.workflow !== options.workflow) {
    problems.push(`${label}:workflow-mismatch: ${attestation.workflow} != ${options.workflow}`);
  }
  const events = WORKFLOW_EVENTS[attestation.workflow];
  if (!events || !events.includes(attestation.event)) {
    problems.push(`${label}:event-unknown: '${attestation.event}' is not a registered event for '${attestation.workflow}'`);
  }
  if (options.event && attestation.event !== options.event) {
    problems.push(`${label}:event-mismatch: ${attestation.event} != ${options.event}`);
  }
  if (!isDigest(attestation.build_environment_digest)) {
    problems.push(`${label}:build-environment-digest-invalid: '${attestation.build_environment_digest}'`);
  }
  if (typeof attestation.rust_toolchain !== 'string' || attestation.rust_toolchain.trim() === '') {
    problems.push(`${label}:rust-toolchain-missing`);
  }
  if (!attestation.pipeline_number) {
    problems.push(`${label}:pipeline-number-missing`);
  } else if (options.pipelineNumber && String(attestation.pipeline_number) !== String(options.pipelineNumber)) {
    problems.push(
      `${label}:pipeline-number-mismatch: ${attestation.pipeline_number} != ${options.pipelineNumber}`,
    );
  }
  // P1: UNCONDITIONAL observed-id binding. The former `pipeline_id ===
  // pipeline_number` exemption let a signer copy the number into the id field
  // and skip the observed API id entirely. There is no exemption now: when the
  // caller observed an id, the attestation must carry exactly that id.
  if (options.pipelineId && String(attestation.pipeline_id) !== String(options.pipelineId)) {
    problems.push(
      `${label}:pipeline-id-mismatch: ${attestation.pipeline_id === undefined ? '<missing>' : attestation.pipeline_id} != ${options.pipelineId}`,
    );
  }
  const artifacts = attestation.artifacts;
  if (!artifacts || typeof artifacts !== 'object' || Array.isArray(artifacts)) {
    problems.push(`${label}:artifacts-missing`);
  } else {
    for (const [name, digest] of Object.entries(artifacts)) {
      if (!isDigest(digest)) {
        problems.push(`${label}:artifact-malformed: '${name}' has digest ${digest}`);
      }
    }
  }
  let matched = 0;
  const local = options.artifacts || [];
  for (const path of local) {
    const full = resolve(options.cwd || ROOT, path);
    if (!existsSync(full)) {
      problems.push(`${label}:local-artifact-missing: '${path}' is not in the workspace`);
      continue;
    }
    const name = basename(path);
    const digest = hashFile(full);
    const attested = artifacts && typeof artifacts === 'object' ? artifacts[name] : undefined;
    if (!attested) {
      problems.push(`${label}:artifact-not-attested: '${path}' (name '${name}') is not covered by the attestation`);
    } else if (attested !== digest) {
      problems.push(`${label}:artifact-mismatch: '${path}' hashes ${digest}, attestation says ${attested}`);
    } else {
      matched += 1;
    }
  }
  if (options.requireSigned || options.keys) {
    const signature = attestation.signature;
    let keys = {};
    try {
      keys = loadKeys(options.keys);
    } catch (error) {
      problems.push(`${label}:keys-missing: ${error.message}`);
    }
    if (!signature || typeof signature !== 'object') {
      if (options.requireSigned) {
        problems.push(`${label}:unsigned: release-grade evidence requires an allowlisted ed25519 signature`);
      }
    } else if (signature.algorithm !== 'ed25519') {
      problems.push(`${label}:signature-invalid: unsupported algorithm '${signature.algorithm}'`);
    } else if (!keys[signature.identity]) {
      problems.push(`${label}:signature-invalid: identity '${signature.identity}' is not on the allowlist`);
    } else {
      // P0: verify with the TRUSTED allowlist key, never the embedded one (same
      // rule as evidence.mjs / crates/updater/src/manifest.rs). Requiring the
      // embedded key to equal the allowlisted key stops an allowlisted identity
      // from being claimed with an attacker-controlled keypair.
      const trustedPublicKey = keys[signature.identity];
      if (signature.public_key !== trustedPublicKey) {
        problems.push(`${label}:signature-invalid: embedded public key does not match allowlist`);
      } else {
        try {
          const ok = cryptoVerify(
            null,
            Buffer.from(signaturePayload(attestation)),
            publicKeyFromRawBase64(trustedPublicKey),
            Buffer.from(signature.value, 'base64'),
          );
          if (!ok) {
            problems.push(`${label}:signature-invalid: signature does not verify`);
          }
        } catch (error) {
          problems.push(`${label}:signature-invalid: ${error.message}`);
        }
      }
    }
  }
  attestation.__matched = matched; // eslint-disable-line no-underscore-dangle
  return problems;
}

function verifyAttestationFile(options) {
  let attestation;
  try {
    attestation = readJsonStrict(options.file);
  } catch (error) {
    console.error(`attestation verify: FAIL (unreadable: ${error.message})`);
    return 1;
  }
  const problems = verifyAttestation(attestation, options);
  if (problems.length === 0) {
    console.log(
      `attestation verify: PASS workflow=${attestation.workflow} event=${attestation.event} ` +
        `source=${attestation.source_sha} pipeline=${attestation.pipeline_number} ` +
        `artifacts-matched=${attestation.__matched} signed=${Boolean(attestation.signature)}`,
    );
    return 0;
  }
  for (const problem of problems) {
    console.error(`attestation problem: ${problem}`);
  }
  return 1;
}

// ---------------------------------------------------------------- keygen

function keygen(options) {
  const { privateKey, publicKey } = generateKeyPairSync('ed25519');
  const rawPublic = publicKey.export({ format: 'der', type: 'spki' }).subarray(-32);
  mkdirSync(resolve(dirname(options.outKey)), { recursive: true });
  writeFileSync(options.outKey, privateKey.export({ format: 'pem', type: 'pkcs8' }));
  const keyId = options.keyId || 'faktor-ci';
  const document = { identities: { [keyId]: { ed25519_public_key: rawPublic.toString('base64') } } };
  writeFileSync(options.outKeys, `${JSON.stringify(document, null, 2)}\n`);
  console.log(`keypair written: ${options.outKey} (private) + ${options.outKeys} (identity ${keyId})`);
  return 0;
}

// -------------------------------------------------------------- selftest

function runSelftest() {
  const failures = [];
  const temp = mkdtempSync(join(tmpdir(), 'faktor-attestation-selftest-'));
  const expect = (name, condition) => {
    if (condition) {
      console.log(`selftest ok: ${name}`);
    } else {
      console.error(`selftest FAIL: ${name}`);
      failures.push(name);
    }
  };
  const expectProblem = (name, problems, code) => {
    const hit = problems.some((problem) => problem.includes(code));
    expect(name, hit);
    if (!hit) {
      console.error(`  problems: ${JSON.stringify(problems)}`);
    }
  };
  try {
    const keyPath = join(temp, 'sign.pem');
    const keysPath = join(temp, 'keys.json');
    const foreignKeysPath = join(temp, 'foreign-keys.json');
    keygen({ outKey: keyPath, outKeys: keysPath, keyId: 'faktor-selftest' });
    keygen({ outKey: join(temp, 'foreign.pem'), outKeys: foreignKeysPath, keyId: 'foreign' });
    const artifactPath = join(temp, 'faktor-selftest.vsix');
    writeFileSync(artifactPath, 'selftest artifact bytes\n');
    const sha = '0123456789abcdef0123456789abcdef01234567';
    const tree = 'fedcba9876543210fedcba9876543210fedcba98';
    const base = {
      workflow: 'trusted',
      event: 'push',
      repo: 'acme/widgets',
      sourceSha: sha,
      treeSha: tree,
      pipelineNumber: '21',
      pipelineId: '211',
      ciImageRef: `node:24@sha256:${'64af'.padEnd(64, '0')}`,
      rustToolchain: '1.98.0',
      artifacts: [artifactPath],
      cwd: ROOT,
      signKey: keyPath,
      keyId: 'faktor-selftest',
    };
    const out = join(temp, 'attestation.json');
    let attestation = createAttestation({ ...base, out });
    const good = verifyAttestation(attestation, {
      sourceSha: sha,
      treeSha: tree,
      workflow: 'trusted',
      event: 'push',
      pipelineNumber: '21',
      pipelineId: '211',
      keys: keysPath,
      requireSigned: true,
      artifacts: [artifactPath],
      cwd: ROOT,
    });
    expect('signed attestation verifies', good.length === 0 && attestation.signature);

    // source/tree/workflow/event binding.
    expectProblem(
      'wrong source SHA is rejected',
      verifyAttestation(attestation, { sourceSha: 'f'.repeat(40), keys: keysPath, requireSigned: true }),
      'source-sha-mismatch',
    );
    expectProblem(
      'wrong tree is rejected',
      verifyAttestation(attestation, { treeSha: 'f'.repeat(40), keys: keysPath, requireSigned: true }),
      'tree-sha-mismatch',
    );
    expectProblem(
      'wrong workflow is rejected',
      verifyAttestation(attestation, { workflow: 'nightly', keys: keysPath, requireSigned: true }),
      'workflow-mismatch',
    );
    expectProblem(
      'wrong event is rejected',
      verifyAttestation(attestation, { workflow: 'trusted', event: 'tag', keys: keysPath, requireSigned: true }),
      'event-mismatch',
    );
    expectProblem(
      'wrong pipeline number is rejected',
      verifyAttestation(attestation, { pipelineNumber: '99', keys: keysPath, requireSigned: true }),
      'pipeline-number-mismatch',
    );
    expectProblem(
      'wrong pipeline id is rejected when it differs from the number',
      verifyAttestation(attestation, { pipelineNumber: '21', pipelineId: '999', keys: keysPath, requireSigned: true }),
      'pipeline-id-mismatch',
    );
    expectProblem(
      'wrong repository is rejected',
      verifyAttestation(attestation, { repo: 'acme/other', keys: keysPath, requireSigned: true }),
      'repository-mismatch',
    );
    const unmutated = { ...attestation };
    delete unmutated.__matched; // verifyAttestation annotates the object it verifies
    expect(
      'matching --repo passes',
      verifyAttestation(unmutated, { repo: 'acme/widgets', keys: keysPath, requireSigned: true }).length === 0,
    );
    // P1 (pre-fix regression): the verifier exempted the observed-id check
    // whenever pipeline_id == pipeline_number, so this attestation (with the
    // number copied into the id field) verified against the observed API id
    // 211 before the fix; it must now be rejected.
    const spoofedPipelineId = createAttestation({ ...base, out: join(temp, 'spoofed-pipeline.json'), pipelineId: '' });
    expectProblem(
      'spoofed pipeline_id==pipeline_number is rejected against the observed id',
      verifyAttestation(spoofedPipelineId, {
        pipelineNumber: '21',
        pipelineId: '211',
        keys: keysPath,
        requireSigned: true,
      }),
      'pipeline-id-mismatch',
    );

    // Artifact binding.
    const tampered = { ...attestation, artifacts: { ...attestation.artifacts } };
    tampered.artifacts[basename(artifactPath)] = `sha256:${'0'.repeat(64)}`;
    expectProblem(
      'tampered attested digest is rejected',
      verifyAttestation(tampered, { keys: keysPath, requireSigned: true, artifacts: [artifactPath], cwd: ROOT }),
      'artifact-mismatch',
    );
    const uncoveredPath = join(temp, 'other.vsix');
    writeFileSync(uncoveredPath, 'not attested\n');
    expectProblem(
      'local artifact not covered is rejected',
      verifyAttestation(attestation, {
        keys: keysPath,
        requireSigned: true,
        artifacts: [uncoveredPath],
        cwd: ROOT,
      }),
      'artifact-not-attested',
    );

    // Signature binding.
    expectProblem(
      'unsigned attestation fails --require-signed',
      verifyAttestation({ ...attestation, signature: null }, { keys: keysPath, requireSigned: true }),
      'unsigned',
    );
    expectProblem(
      'foreign identity is rejected',
      verifyAttestation(attestation, { keys: foreignKeysPath, requireSigned: true }),
      'not on the allowlist',
    );
    const tamperedSignature = {
      ...attestation,
      signature: { ...attestation.signature, value: Buffer.from('not a signature').toString('base64') },
    };
    expectProblem(
      'tampered signature is rejected',
      verifyAttestation(tamperedSignature, { keys: keysPath, requireSigned: true }),
      'signature-invalid',
    );
    const crossSigned = { ...attestation, source_sha: 'a'.repeat(40) };
    expectProblem(
      'payload edit after signing is rejected',
      verifyAttestation(crossSigned, { keys: keysPath, requireSigned: true }),
      'signature-invalid',
    );

    // P0 (pre-fix regression): an attacker keypair carrying the ALLOWLISTED
    // identity name. The unfixed verifier checked signature.identity against
    // the allowlist and then verified with signature.public_key from the
    // attestation, so this fully attacker-controlled object (valid bindings,
    // attacker signature) verified against the honest allowlist; it must now
    // be refused on the embedded key before any crypto check.
    keygen({ outKey: join(temp, 'attacker.pem'), outKeys: join(temp, 'attacker-keys.json'), keyId: 'faktor-selftest' });
    const attackerAttestation = createAttestation({
      ...base,
      out: join(temp, 'attacker-attestation.json'),
      signKey: join(temp, 'attacker.pem'),
      keyId: 'faktor-selftest',
    });
    const allowlistedKey = readJsonStrict(keysPath).identities['faktor-selftest'].ed25519_public_key;
    expect(
      'substitution fixture claims the allowlisted identity with a foreign key',
      attackerAttestation.signature.identity === 'faktor-selftest' &&
        attackerAttestation.signature.public_key !== allowlistedKey,
    );
    expectProblem(
      'allowlisted identity with a substituted key is rejected',
      verifyAttestation(attackerAttestation, {
        repo: 'acme/widgets',
        sourceSha: sha,
        treeSha: tree,
        workflow: 'trusted',
        event: 'push',
        pipelineNumber: '21',
        pipelineId: '211',
        keys: keysPath,
        requireSigned: true,
        artifacts: [artifactPath],
        cwd: ROOT,
      }),
      'embedded public key does not match allowlist',
    );

    // Create-time refusals.
    const refusal = (name, fn, code) => {
      try {
        fn();
        expect(name, false);
      } catch (error) {
        expect(name, error.code === code || error.message.includes(code));
      }
    };
    refusal(
      'undigested CI image is refused',
      () => createAttestation({ ...base, out, ciImageRef: 'node:24' }),
      'not digest-pinned',
    );
    refusal(
      'unknown workflow is refused',
      () => createAttestation({ ...base, out, workflow: 'pr' }),
      'unknown workflow',
    );
    refusal(
      'unknown event is refused',
      () => createAttestation({ ...base, out, event: 'cron' }),
      'attests events',
    );
    refusal(
      'marker/artifact digest drift is refused',
      () => {
        const lanes = join(temp, 'lanes');
        mkdirSync(lanes, { recursive: true });
        writeFileSync(
          join(lanes, 'trusted.json'),
          `${JSON.stringify({
            lane: 'trusted',
            artifacts: [{ path: artifactPath, sha256: `sha256:${'0'.repeat(64)}` }],
          })}\n`,
        );
        createAttestation({ ...base, out, artifacts: [], fromLanes: lanes });
      },
      'marker says',
    );

    // Fail-closed signing: no key -> typed refusal, exit-3 class, NO artifact.
    const failClosedOut = join(temp, 'fail-closed.json');
    refusal(
      'absent signing key is refused fail-closed',
      () =>
        createAttestation({
          ...base,
          out: failClosedOut,
          signKey: '',
          signKeyEnv: 'FAKTOR_ATTEST_SELFTEST_ABSENT',
        }),
      'signing-key-missing',
    );
    expect('fail-closed refusal leaves no attestation artifact', !existsSync(failClosedOut));
    refusal(
      'unreadable --sign-key path is refused fail-closed',
      () => createAttestation({ ...base, out: failClosedOut, signKey: join(temp, 'no-such-key.pem') }),
      'signing-key-missing',
    );
    expect('unreadable key refusal leaves no attestation artifact', !existsSync(failClosedOut));
    const envSignedOut = join(temp, 'env-signed.json');
    process.env.FAKTOR_ATTEST_SELFTEST_KEY = readFileSync(keyPath, 'utf8');
    let envSigned;
    try {
      envSigned = createAttestation({
        ...base,
        out: envSignedOut,
        signKey: '',
        signKeyEnv: 'FAKTOR_ATTEST_SELFTEST_KEY',
      });
    } finally {
      delete process.env.FAKTOR_ATTEST_SELFTEST_KEY;
    }
    expect('--sign-key-env variable signs the attestation', Boolean(envSigned.signature));
    expect(
      'env-signed attestation verifies with --require-signed',
      verifyAttestation(envSigned, {
        sourceSha: sha,
        treeSha: tree,
        workflow: 'trusted',
        event: 'push',
        pipelineNumber: '21',
        keys: keysPath,
        requireSigned: true,
      }).length === 0,
    );

    // Step-image extraction from a workflow file (the real CI path).
    const yamlPath = join(temp, 'trusted.yaml');
    writeFileSync(
      yamlPath,
      [
        'steps:',
        '  - name: image-pins',
        `    image: alpine:3.20@sha256:${'a'.repeat(64)}`,
        '  - name: attestation',
        `    image: node:24@sha256:${'b'.repeat(64)}`,
        '    commands:',
        '      - echo hi',
        '',
      ].join('\n'),
    );
    const extracted = createAttestation({
      ...base,
      out: join(temp, 'extracted.json'),
      ciImageRef: '',
      workflowFile: yamlPath,
      stepName: 'attestation',
    });
    expect(
      'step image digest is extracted from the workflow file',
      extracted.build_environment_digest === `sha256:${'b'.repeat(64)}`,
    );
  } catch (error) {
    console.error(`selftest FAIL: unexpected error: ${error.stack || error.message}`);
    failures.push('unexpected');
  } finally {
    rmSync(temp, { recursive: true, force: true });
  }
  if (failures.length > 0) {
    console.error(`attestation selftest: FAIL (${failures.length} case(s))`);
    return 1;
  }
  console.log('attestation selftest: PASS (binding, artifact, signature and create-time refusal matrix)');
  return 0;
}

// ------------------------------------------------------------------ main

function usage() {
  console.log(`usage: node scripts/certification/attestation.mjs <command> [options]

commands:
  keygen  --out-key PRIVATE.pem --out-keys KEYS.json [--key-id ID]
  create  --out FILE --workflow trusted --event push|tag --repo owner/name
          --source-sha SHA --tree-sha SHA --pipeline-number N [--pipeline-id ID]
          (--ci-image-ref IMAGE | --workflow-file FILE --step-name NAME)
          [--rust-toolchain V | --rust-toolchain-file FILE]
          [--artifact PATH]... [--optional-artifact PATH]... [--from-lanes DIR]
          [--emit-log-block]
          (--sign-key PEM | --sign-key-env VAR) [--key-id ID]
          (signing is FAIL-CLOSED: no key -> typed refusal, no artifact)
  verify  --attestation FILE --source-sha SHA --tree-sha SHA --workflow W
          [--repo OWNER/NAME] [--event E] [--pipeline-number N]
          [--pipeline-id ID]
          --keys KEYS.json [--require-signed] [--artifact PATH]...
  selftest`);
}

function main(argv) {
  const [command, ...args] = argv;
  if (!command || ['-h', '--help', 'help'].includes(command)) {
    usage();
    return command ? 0 : 2;
  }
  try {
    if (command === 'selftest') {
      return runSelftest();
    }
    if (command === 'keygen') {
      const outKey = argValue(args, '--out-key');
      const outKeys = argValue(args, '--out-keys');
      if (!outKey || !outKeys) {
        console.error('keygen: --out-key and --out-keys are required');
        return 2;
      }
      return keygen({ outKey: resolve(outKey), outKeys: resolve(outKeys), keyId: argValue(args, '--key-id') });
    }
    if (command === 'create') {
      const out = argValue(args, '--out');
      if (!out) {
        console.error('create: --out is required');
        return 2;
      }
      const signKeyEnv = argValue(args, '--sign-key-env') || 'FAKTOR_ATTEST_SIGN_KEY_PEM';
      createAttestation({
        out: resolve(out),
        workflow: argValue(args, '--workflow'),
        event: argValue(args, '--event'),
        repo: argValue(args, '--repo'),
        sourceSha: argValue(args, '--source-sha'),
        treeSha: argValue(args, '--tree-sha'),
        pipelineNumber: argValue(args, '--pipeline-number'),
        pipelineId: argValue(args, '--pipeline-id'),
        pipelineUrl: argValue(args, '--pipeline-url'),
        ciImageRef: argValue(args, '--ci-image-ref'),
        workflowFile: argValue(args, '--workflow-file'),
        stepName: argValue(args, '--step-name'),
        rustToolchain: argValue(args, '--rust-toolchain'),
        rustToolchainFile: argValue(args, '--rust-toolchain-file'),
        artifacts: argList(args, '--artifact'),
        optionalArtifacts: argList(args, '--optional-artifact'),
        fromLanes: argValue(args, '--from-lanes'),
        signKey: argValue(args, '--sign-key'),
        signKeyEnv,
        keyId: argValue(args, '--key-id'),
        emitLogBlock: args.includes('--emit-log-block'),
        cwd: argValue(args, '--cwd', ROOT),
      });
      return 0;
    }
    if (command === 'verify') {
      const file = argValue(args, '--attestation');
      if (!file) {
        console.error('verify: --attestation is required');
        return 2;
      }
      return verifyAttestationFile({
        file: resolve(file),
        sourceSha: argValue(args, '--source-sha'),
        treeSha: argValue(args, '--tree-sha'),
        workflow: argValue(args, '--workflow'),
        repo: argValue(args, '--repo'),
        event: argValue(args, '--event'),
        pipelineNumber: argValue(args, '--pipeline-number'),
        pipelineId: argValue(args, '--pipeline-id'),
        keys: argValue(args, '--keys', process.env.FAKTOR_ATTEST_KEYS || ''),
        requireSigned: args.includes('--require-signed'),
        artifacts: argList(args, '--artifact'),
        cwd: argValue(args, '--cwd', ROOT),
      });
    }
  } catch (error) {
    const prefix = error && error.code ? `${error.code}: ` : '';
    console.error(`attestation: ${prefix}${error.message}`);
    return error && error.code === ERROR_CODES.SIGNING_KEY_MISSING ? 3 : 2;
  }
  console.error(`unknown command '${command}'`);
  usage();
  return 2;
}

process.exit(main(process.argv.slice(2)));
