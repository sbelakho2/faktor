#!/usr/bin/env node
// Signed update-manifest assembly (`faktor-update/v1`).
//
// Assembles the distribution manifest from the CERTIFICATION artifacts +
// evidence and signs it with an operator ed25519 key:
//
//   - artifacts come from target/certification/artifacts.json (only entries
//     recorded `built` with a sha256 are shipped; a `fail` packaging run is
//     refused outright);
//   - provenance comes from target/certification/manifest.json (the local
//     certification certificate) and target/certification/evidence/*.json:
//     their digests and levels are embedded, and a certification manifest
//     for a DIFFERENT commit is refused (never foreign evidence);
//   - the signature covers the CANONICAL JSON (object keys sorted
//     recursively) of the manifest WITHOUT its `signature` field — the same
//     canonicalization as scripts/certification/evidence.mjs, so the Rust
//     verifier (crates/updater) accepts byte-for-byte;
//   - `FAKTOR_UPDATE_SIGNING_KEY` (env) supplies the private key: a PEM
//     path, an inline PEM, a 32-byte hex seed, or a base64 seed. When it is
//     ABSENT the manifest is written explicitly UNSIGNED (no `signature`
//     field) and `apply` refuses it with a typed `manifest_unsigned`
//     refusal; `--require-signed` turns that into a non-zero exit.
//
// Usage:
//   node scripts/update-manifest.mjs [assemble] [--artifacts PATH]
//     [--certification PATH] [--certification-optional PATH]
//     [--channel stable|beta|dev] [--version V]
//     [--commit SHA] [--out PATH] [--url-base URL] [--sign-key KEY]
//     [--key-id ID] [--expires-in-days N] [--compat PATH] [--compat-min V]
//     [--signatures PATH] [--require-signed-artifacts]
//     [--require-signed] [--dry-run]
//
// `--signatures PATH` (produced additively by scripts/package-artifacts.sh as
// target/certification/artifact-signatures.json) binds the OS-level signing
// verdicts of THIS run: each record's digest must equal the artifact's
// recorded sha256 (a doctored/mismatched artifact is refused outright), a
// `failed` OS-signing verdict refuses the manifest, and when a certification
// block is present each record's canonical digest is embedded into
// `certification.evidence` as `os_signature.<artifact>` (schema-compatible:
// the value is a 64-hex digest, exactly like the other evidence entries).
// `--require-signed-artifacts` additionally refuses any distributable
// artifact whose OS-signature status is not `signed` — the release policy;
// without it unsigned artifacts are shipped with their explicit UNSIGNED
// marker (usable locally, and the manifest itself can still be ed25519
// signed).
//
// `--certification PATH` binds the provenance strictly (a certificate for
// another commit is refused); `--certification-optional PATH` (used by the
// certify/package path, where the in-run certificate does not exist yet)
// omits a stale certificate with a warning instead of embedding or refusing
// it. Exactly one of the two may be given.
//   node scripts/update-manifest.mjs verify --manifest PATH [--keys PATH]
//   node scripts/update-manifest.mjs selftest
//
// Exit codes: 0 ok, 1 refusal/error, 2 usage.

import { createHash, createPrivateKey, createPublicKey, generateKeyPairSync, sign as cryptoSign, verify as cryptoVerify } from 'node:crypto';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_PATH = fileURLToPath(import.meta.url);
const ROOT = resolve(dirname(SCRIPT_PATH), '..');
const SCHEMA = 'faktor-update/v1';
const ARTIFACTS_SCHEMA = 'faktor-artifacts/v1';
const CHANNELS = ['stable', 'beta', 'dev'];
const DEFAULT_MAX_VALIDITY_DAYS = 30;
const MAX_VALIDITY_DAYS = 365;
const DEFAULT_URL_BASE = 'https://updates.invalid/faktor';
const SIGNATURES_SCHEMA = 'faktor-artifact-signatures/v1';
// The OS-signing verdict vocabulary of scripts/sign-{macos,windows}.sh. A
// `failed` verdict is never publishable; the other statuses ship with an
// explicit marker and are refused only under --require-signed-artifacts.
const OS_SIGNATURE_STATUSES = new Set(['signed', 'unsigned', 'skipped', 'not_applicable']);
// Mirrors crates/updater's MAX_CERTIFICATION_EVIDENCE_ENTRIES (the manifest
// schema is strict there; this script must never assemble a refused file).
const MAX_CERTIFICATION_EVIDENCE = 16;

function usage(code = 0) {
  const text = readFileSync(SCRIPT_PATH, 'utf8')
    .split('\n')
    .filter((line) => line.startsWith('//') || line.startsWith('#!'))
    .map((line) => line.replace(/^\/\/ ?/, '').replace(/^#!.*/, ''))
    .join('\n');
  (code === 0 ? process.stdout : process.stderr).write(`${text}\n`);
  process.exit(code);
}

function argValue(args, name, fallback = '') {
  const index = args.indexOf(name);
  if (index < 0) return fallback;
  if (index + 1 >= args.length) {
    console.error(`[update-manifest] ${name} needs a value`);
    process.exit(2);
  }
  return args[index + 1];
}

function flag(args, name) {
  return args.includes(name);
}

function sha256Hex(buffer) {
  return createHash('sha256').update(buffer).digest('hex');
}

function isDigest(value) {
  return typeof value === 'string' && /^[0-9a-f]{64}$/.test(value);
}

function isSha(value) {
  return typeof value === 'string' && /^[0-9a-f]{40}$/.test(value);
}

function headCommit() {
  try {
    return execFileSync('git', ['-C', ROOT, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  } catch {
    return '';
  }
}

// The canonical signing payload: object keys sorted recursively, compact
// separators, `undefined` rendered as null. Mirrors evidence.mjs and
// crates/updater/src/manifest.rs.
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

function readJsonStrict(path, label) {
  let text;
  try {
    text = readFileSync(path, 'utf8');
  } catch (error) {
    throw new Error(`${label} ${path} is unreadable: ${error.message}`);
  }
  try {
    return JSON.parse(text);
  } catch (error) {
    throw new Error(`${label} ${path} is not JSON: ${error.message}`);
  }
}

// ------------------------------------------------------------------ keys

const ED25519_PKCS8_PREFIX = '302e020100300506032b657004220420';
const ED25519_SPKI_PREFIX = '302a300506032b6570032100';

function privateKeyFromEnv(raw) {
  const value = String(raw || '').trim();
  if (value === '') return null;
  if (existsSync(value)) {
    return createPrivateKey(readFileSync(value));
  }
  if (value.startsWith('-----BEGIN')) {
    return createPrivateKey(value);
  }
  if (/^[0-9a-fA-F]{64}$/.test(value)) {
    return seedKey(Buffer.from(value, 'hex'));
  }
  const decoded = Buffer.from(value, 'base64');
  if (decoded.length === 32) {
    return seedKey(decoded);
  }
  throw new Error('FAKTOR_UPDATE_SIGNING_KEY is neither a PEM path, an inline PEM, a 32-byte hex seed nor a 32-byte base64 seed');
}

function seedKey(seed) {
  return createPrivateKey({
    key: Buffer.concat([Buffer.from(ED25519_PKCS8_PREFIX, 'hex'), seed]),
    format: 'der',
    type: 'pkcs8',
  });
}

function rawPublicKey(privateKey) {
  return createPublicKey(privateKey).export({ format: 'der', type: 'spki' }).subarray(12).toString('base64');
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

function signManifest(manifest, privateKey, keyId) {
  const payload = Buffer.from(canonicalJson(manifest), 'utf8');
  const value = cryptoSign(null, payload, privateKey);
  return {
    ...manifest,
    signature: {
      algorithm: 'ed25519',
      identity: keyId,
      public_key: rawPublicKey(privateKey),
      value: value.toString('base64'),
    },
  };
}

function verifyManifestSignature(manifest, allowedIdentities) {
  const signature = manifest && manifest.signature;
  if (!signature || typeof signature !== 'object') {
    return { signed: false, ok: false, reason: 'the manifest is UNSIGNED' };
  }
  if (signature.algorithm !== 'ed25519') {
    return { signed: true, ok: false, reason: `unsupported signature algorithm '${signature.algorithm}'` };
  }
  if (allowedIdentities && !(signature.identity in allowedIdentities)) {
    return { signed: true, ok: false, reason: `identity '${signature.identity}' is not on the allowlist` };
  }
  if (allowedIdentities && allowedIdentities[signature.identity] !== signature.public_key) {
    return { signed: true, ok: false, reason: `identity '${signature.identity}' does not match its allowlisted key` };
  }
  const withoutSignature = { ...manifest };
  delete withoutSignature.signature;
  try {
    const ok = cryptoVerify(
      null,
      Buffer.from(canonicalJson(withoutSignature), 'utf8'),
      publicKeyFromRawBase64(signature.public_key),
      Buffer.from(signature.value, 'base64'),
    );
    return ok
      ? { signed: true, ok: true, reason: 'signature verified' }
      : { signed: true, ok: false, reason: 'the signature does not verify (tampered?)' };
  } catch (error) {
    return { signed: true, ok: false, reason: `signature verification failed: ${error.message}` };
  }
}

function loadAllowlist(path) {
  const parsed = readJsonStrict(path, 'key allowlist');
  const identities = parsed.identities || parsed;
  const out = {};
  for (const [identity, value] of Object.entries(identities || {})) {
    if (typeof value === 'string') {
      out[identity] = value;
    } else if (value && typeof value.ed25519_public_key === 'string') {
      out[identity] = value.ed25519_public_key;
    } else {
      throw new Error(`key allowlist entry '${identity}' is malformed`);
    }
  }
  return out;
}

// ------------------------------------------------------- manifest assembly

function versionOf(artifacts, explicit) {
  if (explicit) return explicit;
  if (artifacts && typeof artifacts.version === 'string' && artifacts.version !== '') {
    return artifacts.version;
  }
  throw new Error('no version: pass --version or provide artifacts.json with a version');
}

function artifactOsArch(entry, artifacts) {
  switch (entry.kind) {
    case 'daemon-bundle':
      return { os: artifacts.os || 'unknown', arch: artifacts.arch || 'unknown' };
    case 'vsix':
    case 'jetbrains-plugin':
      // Extension artifacts are distributed to every host; the daemon
      // updater only ever selects its own os/arch bundle.
      return { os: 'any', arch: 'any' };
    default:
      return null;
  }
}

// ------------------------------------------- OS-signature records (additive)

// Read target/certification/artifact-signatures.json (written by
// scripts/package-artifacts.sh step 4.5). Returns a Map artifactName ->
// record. An absent path is an empty map (no OS signing was attempted).
function readSignatureRecords(path) {
  const records = new Map();
  if (!path) {
    return records;
  }
  if (!existsSync(path)) {
    throw new Error(`OS-signature records ${path} do not exist`);
  }
  const parsed = readJsonStrict(path, 'OS-signature records');
  if (parsed.schema !== SIGNATURES_SCHEMA) {
    throw new Error(`OS-signature records schema '${parsed.schema}' != '${SIGNATURES_SCHEMA}'`);
  }
  for (const row of parsed.artifacts || []) {
    if (!row || typeof row.name !== 'string' || row.name === '') {
      throw new Error('OS-signature records carry an entry without an artifact name');
    }
    if (records.has(row.name)) {
      throw new Error(`OS-signature records name artifact '${row.name}' twice`);
    }
    records.set(row.name, row);
  }
  return records;
}

// Validate one signature record against its artifacts.json entry. The record
// must describe the SAME bytes the manifest will ship (the recorded digest
// equality is the anti-doctoring step), carry a known status, and never
// report `failed`. Returns the canonical digest of the record (bound into
// the certification evidence when one exists).
function validateSignatureRecord(entry, record, requireSigned) {
  const signature = record.signature;
  if (!signature || typeof signature !== 'object' || Array.isArray(signature)) {
    throw new Error(`artifact ${entry.name} carries an OS-signature record without a signature object`);
  }
  if (!isDigest(record.sha256)) {
    throw new Error(`artifact ${entry.name} OS-signature record has no 64-hex digest`);
  }
  if (record.sha256 !== entry.sha256) {
    throw new Error(
      `artifact ${entry.name} OS-signature record covers digest ${record.sha256} but artifacts.json records ${entry.sha256}: refusing (doctored or stale artifact)`,
    );
  }
  const status = signature.status;
  if (!OS_SIGNATURE_STATUSES.has(status)) {
    throw new Error(
      `artifact ${entry.name} OS-signature status '${status}' is not one of ${[...OS_SIGNATURE_STATUSES].join('|')}`,
    );
  }
  if (requireSigned && status !== 'signed') {
    throw new Error(
      `artifact ${entry.name} is not OS-signed (status '${status}'): --require-signed-artifacts refuses an unsigned release`,
    );
  }
  return sha256Hex(Buffer.from(canonicalJson(signature), 'utf8'));
}

function assembleArtifacts(artifacts, urlBase, version, signatureRecords, requireSignedArtifacts) {
  if (!artifacts || typeof artifacts !== 'object') {
    throw new Error('artifacts.json is not an object');
  }
  if (artifacts.schema !== ARTIFACTS_SCHEMA) {
    throw new Error(`artifacts.json schema '${artifacts.schema}' != '${ARTIFACTS_SCHEMA}'`);
  }
  if (artifacts.status !== 'pass') {
    throw new Error(`artifacts.json status '${artifacts.status}' != 'pass': refusing to publish an update manifest from a failed packaging run`);
  }
  const rows = [];
  const skipped = [];
  const signatureEvidence = new Map();
  for (const entry of artifacts.artifacts || []) {
    if (!entry || entry.status !== 'built') {
      skipped.push(`${entry && entry.name ? entry.name : 'unnamed'}: not built`);
      continue;
    }
    if (!isDigest(entry.sha256)) {
      throw new Error(`artifact ${entry.name} is recorded built without a sha256 digest`);
    }
    if (typeof entry.name !== 'string' || entry.name === '' || entry.name.includes('/')) {
      throw new Error(`artifact name ${JSON.stringify(entry.name)} is not a plain file name`);
    }
    const osArch = artifactOsArch(entry, artifacts);
    if (!osArch) {
      skipped.push(`${entry.name}: kind '${entry.kind}' is not distributed through the updater`);
      continue;
    }
    const record = signatureRecords.get(entry.name);
    if (record) {
      const signatureDigest = validateSignatureRecord(entry, record, requireSignedArtifacts);
      signatureEvidence.set(`os_signature.${entry.name}`, signatureDigest);
      if (record.signature.status !== 'signed') {
        console.error(
          `[update-manifest] note: ${entry.name} ships with OS-signature status '${record.signature.status}' (${record.signature.detail || 'no detail'})`,
        );
      }
    } else if (requireSignedArtifacts) {
      throw new Error(
        `artifact ${entry.name} has no OS-signature record: --require-signed-artifacts refuses an unsigned release`,
      );
    } else {
      console.error(
        `[update-manifest] note: ${entry.name} has no OS-signature record (no signing driver ran); it ships explicitly unsigned`,
      );
    }
    const row = {
      name: entry.name,
      os: osArch.os,
      arch: osArch.arch,
      sha256: entry.sha256,
      url: `${urlBase.replace(/\/+$/, '')}/${encodeURIComponent(version)}/${encodeURIComponent(entry.name)}`,
    };
    if (Number.isFinite(entry.size) && entry.size > 0) {
      row.size = entry.size;
    }
    rows.push(row);
  }
  if (rows.length === 0) {
    throw new Error('no distributable artifact was recorded built in artifacts.json');
  }
  rows.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
  return { rows, skipped, signatureEvidence };
}

function certificationBlock(certificationPath, commit) {
  if (!existsSync(certificationPath)) {
    return null;
  }
  const raw = readFileSync(certificationPath);
  const parsed = JSON.parse(raw.toString('utf8'));
  if (typeof parsed.commit !== 'string' || parsed.commit !== commit) {
    throw new Error(
      `certification manifest ${certificationPath} is for commit ${parsed.commit || 'unknown'}, not ${commit}: refusing foreign evidence`,
    );
  }
  const level = parsed.certification_level || parsed.level || 'none';
  if (!['none', 'local_offline', 'release'].includes(level)) {
    throw new Error(`certification level '${level}' is not none|local_offline|release`);
  }
  const evidence = {};
  for (const kind of ['local_offline', 'cross_platform_lanes', 'real_provider']) {
    const file = join(dirname(certificationPath), 'evidence', `${kind}.json`);
    if (existsSync(file)) {
      evidence[kind] = sha256Hex(readFileSync(file));
    }
  }
  return {
    level,
    commit,
    manifest_sha256: sha256Hex(raw),
    evidence,
  };
}

function compatibilityBlock(version, compatPath, compatMin) {
  if (compatPath) {
    const parsed = readJsonStrict(compatPath, 'compatibility');
    for (const component of ['cli', 'daemon', 'vscode', 'jetbrains', 'schema']) {
      if (!(component in parsed)) {
        throw new Error(`compatibility file is missing the '${component}' range`);
      }
    }
    return parsed;
  }
  const min = compatMin || '0.0.0';
  const bounded = { min, max: version };
  const open = { min: '*', max: '*' };
  return {
    cli: { ...bounded },
    daemon: { ...bounded },
    vscode: { ...open },
    jetbrains: { ...open },
    schema: { min: 1, max: 1 },
  };
}

function assembleManifest(options) {
  const artifacts = readJsonStrict(options.artifactsPath, 'artifacts');
  const version = versionOf(artifacts, options.version);
  const commit = options.commit || artifacts.commit || '';
  if (!isSha(commit)) {
    throw new Error(`commit '${commit}' is not a 40-hex sha`);
  }
  const head = headCommit();
  if (head && commit !== head) {
    throw new Error(`commit ${commit} is not HEAD (${head}): refusing to sign an update manifest for another commit`);
  }
  const signatureRecords = readSignatureRecords(options.signaturesPath);
  const { rows, skipped, signatureEvidence } = assembleArtifacts(
    artifacts,
    options.urlBase,
    version,
    signatureRecords,
    options.requireSignedArtifacts,
  );
  for (const note of skipped) {
    console.error(`[update-manifest] skip: ${note}`);
  }
  const issuedAt = Date.now();
  const expiresAt = issuedAt + options.expiresInDays * 24 * 60 * 60 * 1000;
  const manifest = {
    schema: SCHEMA,
    channel: options.channel,
    version,
    commit,
    artifacts: rows,
    compatibility: compatibilityBlock(version, options.compatPath, options.compatMin),
    issued_at: issuedAt,
    expires_at: expiresAt,
  };
  let certification = null;
  if (options.certificationOptionalPath) {
    const path = resolve(ROOT, options.certificationOptionalPath);
    if (!existsSync(path)) {
      console.error(`[update-manifest] note: no certification certificate at ${path} yet; the manifest carries no certification provenance`);
    } else {
      try {
        certification = certificationBlock(path, commit);
      } catch (error) {
        console.error(`[update-manifest] note: ignoring the certification certificate at ${path}: ${error.message}`);
      }
    }
  } else {
    certification = certificationBlock(options.certificationPath, commit);
  }
  // Bind the OS-signature records into the signed payload. The manifest
  // schema is strict (crates/updater), so the records ride the certification
  // block's evidence map as 64-hex canonical digests — exactly the value
  // shape every other evidence entry uses.
  if (certification && signatureEvidence.size > 0) {
    for (const [key, digest] of signatureEvidence) {
      if (key.length > 64) {
        throw new Error(`OS-signature evidence key ${JSON.stringify(key)} exceeds 64 bytes`);
      }
      certification.evidence[key] = digest;
    }
    if (Object.keys(certification.evidence).length > MAX_CERTIFICATION_EVIDENCE) {
      throw new Error(
        `certification evidence would hold ${Object.keys(certification.evidence).length} entries (max ${MAX_CERTIFICATION_EVIDENCE}): refusing to assemble a manifest the Rust verifier rejects`,
      );
    }
  } else if (signatureEvidence.size > 0) {
    console.error(
      `[update-manifest] note: ${signatureEvidence.size} OS-signature record(s) are recorded in artifacts.json/artifact-signatures.json but not embedded (no certification certificate); the artifact digests in this signed manifest cover the signed bytes`,
    );
  }
  if (certification) {
    manifest.certification = certification;
  }
  return manifest;
}

// ------------------------------------------------------------------ modes

function modeAssemble(args) {
  const options = {
    artifactsPath: resolve(ROOT, argValue(args, '--artifacts', join(ROOT, 'target/certification/artifacts.json'))),
    certificationPath: resolve(ROOT, argValue(args, '--certification', join(ROOT, 'target/certification/manifest.json'))),
    certificationOptionalPath: argValue(args, '--certification-optional', ''),
    outPath: resolve(ROOT, argValue(args, '--out', join(ROOT, 'target/certification/update-manifest.json'))),
    channel: argValue(args, '--channel', 'stable'),
    version: argValue(args, '--version', ''),
    commit: argValue(args, '--commit', ''),
    urlBase: argValue(args, '--url-base', process.env.FAKTOR_UPDATE_URL_BASE || DEFAULT_URL_BASE),
    keyId: argValue(args, '--key-id', process.env.FAKTOR_UPDATE_KEY_ID || 'operator'),
    expiresInDays: Number(argValue(args, '--expires-in-days', String(DEFAULT_MAX_VALIDITY_DAYS))),
    compatPath: argValue(args, '--compat', ''),
    compatMin: argValue(args, '--compat-min', ''),
    signaturesPath: argValue(args, '--signatures', ''),
    requireSignedArtifacts: flag(args, '--require-signed-artifacts'),
    requireSigned: flag(args, '--require-signed'),
    dryRun: flag(args, '--dry-run'),
    signKey: argValue(args, '--sign-key', process.env.FAKTOR_UPDATE_SIGNING_KEY || ''),
  };
  if (!CHANNELS.includes(options.channel)) {
    console.error(`[update-manifest] --channel must be one of ${CHANNELS.join('|')}`);
    process.exit(2);
  }
  if (!Number.isInteger(options.expiresInDays) || options.expiresInDays < 1 || options.expiresInDays > MAX_VALIDITY_DAYS) {
    console.error(`[update-manifest] --expires-in-days must be 1..=${MAX_VALIDITY_DAYS}`);
    process.exit(2);
  }
  if (!String(options.urlBase).startsWith('https://') && !String(options.urlBase).startsWith('http://')) {
    console.error('[update-manifest] --url-base must be an http(s) URL');
    process.exit(2);
  }
  if (options.certificationOptionalPath && flag(args, '--certification')) {
    console.error('[update-manifest] --certification and --certification-optional are mutually exclusive');
    process.exit(2);
  }
  if (String(options.urlBase) === DEFAULT_URL_BASE) {
    console.error(`[update-manifest] WARNING: no --url-base/FAKTOR_UPDATE_URL_BASE configured; using the placeholder ${DEFAULT_URL_BASE} (downloads will not resolve)`);
  }

  let manifest;
  try {
    manifest = assembleManifest(options);
  } catch (error) {
    console.error(`[update-manifest] REFUSED: ${error.message}`);
    process.exit(1);
  }

  const privateKey = options.signKey ? privateKeyFromEnv(options.signKey) : null;
  let signed = manifest;
  if (privateKey) {
    signed = signManifest(manifest, privateKey, options.keyId);
    const verdict = verifyManifestSignature(signed, null);
    if (!verdict.ok) {
      console.error(`[update-manifest] REFUSED: self-verification failed: ${verdict.reason}`);
      process.exit(1);
    }
  } else {
    console.error(
      '[update-manifest] UNSIGNED: FAKTOR_UPDATE_SIGNING_KEY is not set, so the manifest carries NO signature and `apply` will refuse it with a typed manifest_unsigned refusal',
    );
    if (options.requireSigned) {
      console.error('[update-manifest] REFUSED: --require-signed and no signing key');
      process.exit(1);
    }
  }

  const rendered = `${JSON.stringify(signed, null, 2)}\n`;
  if (options.dryRun) {
    process.stdout.write(rendered);
    return;
  }
  writeFileSync(options.outPath, rendered);
  const digest = sha256Hex(Buffer.from(rendered, 'utf8'));
  console.log(
    `[update-manifest] ${privateKey ? `signed by ${options.keyId}` : 'UNSIGNED'} channel=${options.channel} version=${signed.version} artifacts=${signed.artifacts.length} commit=${signed.commit}`,
  );
  console.log(`[update-manifest] wrote ${options.outPath} (sha256 ${digest})`);
}

function modeVerify(args) {
  const manifestPath = argValue(args, '--manifest', '');
  if (!manifestPath) {
    console.error('[update-manifest] verify needs --manifest PATH');
    process.exit(2);
  }
  const manifest = readJsonStrict(resolve(ROOT, manifestPath), 'manifest');
  if (manifest.schema !== SCHEMA) {
    console.error(`[update-manifest] REFUSED: schema '${manifest.schema}' != '${SCHEMA}'`);
    process.exit(1);
  }
  const keysPath = argValue(args, '--keys', '');
  const allowlist = keysPath ? loadAllowlist(resolve(ROOT, keysPath)) : null;
  const verdict = verifyManifestSignature(manifest, allowlist);
  if (!verdict.ok) {
    console.error(`[update-manifest] REFUSED: ${verdict.reason}`);
    process.exit(1);
  }
  console.log(`[update-manifest] verified: identity=${manifest.signature.identity} version=${manifest.version} channel=${manifest.channel}`);
}

function modeSelftest() {
  const dir = mkdtempSync(join(tmpdir(), 'faktor-update-selftest.'));
  let failures = 0;
  const expect = (label, condition) => {
    if (!condition) {
      failures += 1;
      console.error(`[update-manifest selftest] FAIL: ${label}`);
    } else {
      console.log(`[update-manifest selftest] ok: ${label}`);
    }
  };
  try {
    const commit = headCommit() || 'a'.repeat(40);
    const { privateKey, publicKey } = generateKeyPairSync('ed25519');
    const publicB64 = publicKey.export({ format: 'der', type: 'spki' }).subarray(12).toString('base64');
    const artifactsPath = join(dir, 'artifacts.json');
    writeFileSync(
      artifactsPath,
      JSON.stringify({
        schema: ARTIFACTS_SCHEMA,
        status: 'pass',
        commit,
        version: '9.9.9',
        os: 'darwin',
        arch: 'arm64',
        artifacts: [
          { name: 'faktor-cli-9.9.9-darwin-arm64.tar.gz', kind: 'daemon-bundle', status: 'built', sha256: 'a'.repeat(64), size: 1234 },
          { name: 'faktor-9.9.9.vsix', kind: 'vsix', status: 'built', sha256: 'b'.repeat(64), size: 42 },
          { name: 'broken.tar.gz', kind: 'daemon-bundle', status: 'failed', sha256: null },
        ],
      }),
    );
    const keyPath = join(dir, 'key.pem');
    writeFileSync(keyPath, privateKey.export({ type: 'pkcs8', format: 'pem' }));
    const outPath = join(dir, 'update-manifest.json');
    const run = (extra, env = {}) =>
      spawnSync(process.execPath, [SCRIPT_PATH, '--artifacts', artifactsPath, '--commit', commit, '--out', outPath, '--url-base', 'https://mirror.test/faktor', ...extra], {
        encoding: 'utf8',
        env: { ...process.env, FAKTOR_UPDATE_SIGNING_KEY: '', ...env },
      });

    // Unsigned assembly is explicit and refuses to claim otherwise.
    const unsignedRun = run([]);
    expect('unsigned assembly succeeds and says UNSIGNED', unsignedRun.status === 0 && String(unsignedRun.stderr).includes('UNSIGNED'));
    const unsigned = JSON.parse(readFileSync(outPath, 'utf8'));
    expect('unsigned manifest carries no signature', !('signature' in unsigned));
    expect(
      'unsigned manifest is refused by verify',
      spawnSync(process.execPath, [SCRIPT_PATH, 'verify', '--manifest', outPath], { encoding: 'utf8' }).status !== 0,
    );
    expect('--require-signed refuses an unsigned manifest', run(['--require-signed']).status !== 0);
    expect('artifact mapping covers bundle + extension, skips failed', unsigned.artifacts.length === 2
      && unsigned.artifacts.some((a) => a.os === 'darwin' && a.arch === 'arm64')
      && unsigned.artifacts.some((a) => a.os === 'any' && a.arch === 'any'));

    // Signed assembly verifies, and the signature covers every field.
    expect('signed assembly succeeds', run(['--sign-key', keyPath, '--key-id', 'selftest']).status === 0);
    const signed = JSON.parse(readFileSync(outPath, 'utf8'));
    expect('signed manifest verifies', verifyManifestSignature(signed, { selftest: publicB64 }).ok);
    expect(
      'the verify mode accepts the signed manifest',
      spawnSync(process.execPath, [SCRIPT_PATH, 'verify', '--manifest', outPath], { encoding: 'utf8' }).status === 0,
    );
    const tampered = { ...signed, version: '9.9.10' };
    expect('tampered manifest is refused', !verifyManifestSignature(tampered, { selftest: publicB64 }).ok);
    const wrongIdentity = verifyManifestSignature(signed, { other: publicB64 });
    expect('unknown identity is refused by an allowlist', !wrongIdentity.ok);
    // Canonical form is key-order independent (recursively): reordering the
    // object keys does not change the signed payload.
    const reordered = {};
    for (const key of Object.keys(signed).reverse()) {
      const value = signed[key];
      if (value && typeof value === 'object' && !Array.isArray(value)) {
        const nested = {};
        for (const inner of Object.keys(value).reverse()) {
          nested[inner] = value[inner];
        }
        reordered[key] = nested;
      } else {
        reordered[key] = value;
      }
    }
    expect(
      'canonical payload is key-order independent',
      canonicalJson(manifestWithoutSignature(reordered)) === canonicalJson(manifestWithoutSignature(signed)),
    );

    // --- OS-signature records (additive packaging step 4.5) ---------------
    const signaturesPath = join(dir, 'artifact-signatures.json');
    const signatureRecord = (name, sha256, signature) => ({ name, sha256, status: 'built', signature });
    const signatureRecords = (bundleOverrides = {}, vsixOverrides = {}) => ({
      schema: SIGNATURES_SCHEMA,
      commit,
      os: 'darwin',
      arch: 'arm64',
      artifacts: [
        signatureRecord('faktor-cli-9.9.9-darwin-arm64.tar.gz', 'a'.repeat(64), {
          tool: 'macos', status: 'signed', scope: 'inner:bin/faktor-cli', detail: 'codesigned + notarized', marker: null, codesigned: true, notarized: true, stapled: false, ...bundleOverrides,
        }),
        signatureRecord('faktor-9.9.9.vsix', 'b'.repeat(64), {
          tool: 'macos', status: 'not_applicable', scope: 'file', detail: 'extension zip', marker: null, codesigned: false, notarized: false, stapled: false, ...vsixOverrides,
        }),
      ],
    });
    const writeSignatures = (bundleOverrides = {}, vsixOverrides = {}) => {
      writeFileSync(signaturesPath, JSON.stringify(signatureRecords(bundleOverrides, vsixOverrides)));
    };
    writeSignatures();
    expect(
      'signature-bound assembly succeeds',
      run(['--signatures', signaturesPath, '--sign-key', keyPath, '--key-id', 'selftest']).status === 0,
    );
    const signedWithSignatures = JSON.parse(readFileSync(outPath, 'utf8'));
    expect('signature-bound manifest still verifies', verifyManifestSignature(signedWithSignatures, { selftest: publicB64 }).ok);

    // A doctored artifact digest (artifacts.json moved, records did not) is
    // refused: the signature record no longer covers the shipped bytes.
    const doctoredArtifacts = JSON.parse(readFileSync(artifactsPath, 'utf8'));
    doctoredArtifacts.artifacts[0].sha256 = 'c'.repeat(64);
    writeFileSync(artifactsPath, JSON.stringify(doctoredArtifacts));
    expect(
      'a doctored artifact digest is refused against the recorded signature',
      run(['--signatures', signaturesPath, '--sign-key', keyPath]).status !== 0,
    );
    // The same doctoring is caught by the digest step even without signing.
    expect(
      'a doctored artifact digest is refused unsigned too',
      run(['--signatures', signaturesPath]).status !== 0,
    );
    doctoredArtifacts.artifacts[0].sha256 = 'a'.repeat(64);
    writeFileSync(artifactsPath, JSON.stringify(doctoredArtifacts));

    // A `failed` OS-signing verdict can never be published.
    writeSignatures({ status: 'failed', detail: 'codesign refused' });
    expect(
      'a failed OS-signing verdict refuses the manifest',
      run(['--signatures', signaturesPath, '--sign-key', keyPath]).status !== 0,
    );

    // --require-signed-artifacts refuses an unsigned/not-applicable release
    // and accepts an all-signed one.
    writeSignatures();
    expect(
      '--require-signed-artifacts refuses a not_applicable artifact',
      run(['--signatures', signaturesPath, '--sign-key', keyPath, '--require-signed-artifacts']).status !== 0,
    );
    writeSignatures({}, { status: 'signed', detail: 'fake signed' });
    expect(
      '--require-signed-artifacts accepts an all-signed release',
      run(['--signatures', signaturesPath, '--sign-key', keyPath, '--require-signed-artifacts']).status === 0,
    );
    // A missing signature records file is a loud refusal (never "unsigned by
    // accident").
    expect(
      'a missing signature records file is refused',
      run(['--signatures', join(dir, 'absent-signatures.json')]).status !== 0,
    );

    // --- OS-signature digest binding into the certification evidence ------
    writeSignatures();
    writeFileSync(join(dir, 'cert-good.json'), JSON.stringify({ commit, certification_level: 'release' }));
    expect(
      'certification + signature records assemble',
      run(['--signatures', signaturesPath, '--certification', join(dir, 'cert-good.json'), '--sign-key', keyPath, '--key-id', 'selftest']).status === 0,
    );
    const certified = JSON.parse(readFileSync(outPath, 'utf8'));
    const bundleEvidenceKey = 'os_signature.faktor-cli-9.9.9-darwin-arm64.tar.gz';
    expect(
      'the OS-signature digest is bound into the signed certification evidence',
      isDigest(certified.certification?.evidence?.[bundleEvidenceKey]),
    );
    expect('the certified manifest verifies', verifyManifestSignature(certified, { selftest: publicB64 }).ok);
    const expectedSignatureDigest = sha256Hex(
      Buffer.from(
        canonicalJson(signatureRecords().artifacts[0].signature),
        'utf8',
      ),
    );
    expect(
      'the bound digest is the canonical digest of the recorded verdict',
      certified.certification?.evidence?.[bundleEvidenceKey] === expectedSignatureDigest,
    );
    // Tampering the verdict changes the bound digest (the anchor a verifier
    // compares artifacts.json against).
    writeSignatures({ detail: 'tampered detail after packaging' });
    run(['--signatures', signaturesPath, '--certification', join(dir, 'cert-good.json'), '--sign-key', keyPath]);
    const tamperedCertified = JSON.parse(readFileSync(outPath, 'utf8'));
    expect(
      'tampering the recorded verdict changes the bound digest',
      tamperedCertified.certification?.evidence?.[bundleEvidenceKey] !== expectedSignatureDigest,
    );

    // A failed packaging run can never be signed.
    writeFileSync(artifactsPath, JSON.stringify({ schema: ARTIFACTS_SCHEMA, status: 'fail', commit, version: '9.9.9', os: 'darwin', arch: 'arm64', artifacts: [] }));
    expect('a failed packaging run is refused', run(['--sign-key', keyPath]).status !== 0);

    // Foreign certification evidence is refused.
    writeFileSync(
      join(dir, 'cert.json'),
      JSON.stringify({ commit: 'b'.repeat(40), certification_level: 'release' }),
    );
    expect(
      'foreign certification evidence is refused',
      run(['--sign-key', keyPath, '--certification', join(dir, 'cert.json')]).status !== 0,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  if (failures > 0) {
    console.error(`[update-manifest selftest] FAIL (${failures} assertion(s))`);
    process.exit(1);
  }
  console.log('[update-manifest selftest] PASS');
}

function manifestWithoutSignature(manifest) {
  const copy = { ...manifest };
  delete copy.signature;
  return copy;
}

const args = process.argv.slice(2);
const mode = args[0] && !args[0].startsWith('--') ? args[0] : 'assemble';
if (mode === '--help' || mode === '-h') {
  usage(0);
}
switch (mode) {
  case 'assemble':
    modeAssemble(args);
    break;
  case 'verify':
    modeVerify(args);
    break;
  case 'selftest':
    modeSelftest();
    break;
  default:
    console.error(`[update-manifest] unknown mode '${mode}'`);
    usage(2);
}
