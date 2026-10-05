#!/usr/bin/env node
// Frozen-contract generator entry point and node-only integrity verifier.
//
// The canonical files under docs/contracts/ are generated from the COMPILED
// Rust vocabularies by the `faktor-contracts` crate:
//
//   cargo run -p faktor-contracts -- check   (regenerate + compare; exit 1 on drift)
//   cargo run -p faktor-contracts -- write   (only for a deliberate contract change)
//
// This script:
//   verify (default)  node-only: re-hash every frozen file against
//                     manifest.json and recompute the aggregate contract
//                     digest. No cargo required, so the node-only
//                     certificate lane can bind the digest into evidence.
//   selftest          hermetic mutation matrix: rename/reorder/tamper/extra/
//                     missing/duplicate-key must all be refused.
//   check             spawn the cargo drift gate (cargo-capable lanes/devs).
//   write             spawn the cargo generator, then verify.
//
// Usage:
//   node scripts/contracts/emit.mjs [verify|selftest|check|write]
//        [--root DIR] [--print-digest] [--json]
//
// Exit codes: 0 pass; 1 verification failure; 2 usage.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
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
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const CONTRACT_DIR = 'docs/contracts';
const MANIFEST = `${CONTRACT_DIR}/manifest.json`;
const MANIFEST_SCHEMA = 'faktor-frozen-contracts-manifest/v1';
const CONTRACT_SCHEMA = 'faktor-frozen-contract/v1';
const DIGEST_RE = /^sha256:[0-9a-f]{64}$/;

function sha256Hex(data) {
  return createHash('sha256').update(data).digest('hex');
}

function usage() {
  console.error(
    'usage: node scripts/contracts/emit.mjs [verify|selftest|check|write] ' +
      '[--root DIR] [--print-digest] [--json]\n' +
      '  verify   re-hash the frozen files against manifest.json (no cargo needed)\n' +
      '  selftest hermetic rename/reorder/tamper mutation matrix\n' +
      '  check    spawn `cargo run -p faktor-contracts -- check`\n' +
      '  write    spawn `cargo run -p faktor-contracts -- write`, then verify',
  );
}

// ------------------------------------------------------------------ strict JSON

/** Reject duplicate object keys: JSON.parse silently keeps the last one. */
function findDuplicateKey(text) {
  const stack = [];
  let i = 0;
  const isWs = (ch) => ch === ' ' || ch === '\t' || ch === '\n' || ch === '\r';
  const skipString = (from) => {
    let j = from + 1;
    while (j < text.length) {
      if (text[j] === '\\') j += 2;
      else if (text[j] === '"') return j + 1;
      else j += 1;
    }
    return j;
  };
  while (i < text.length) {
    const ch = text[i];
    if (isWs(ch)) {
      i += 1;
      continue;
    }
    if (ch === '"') {
      const end = skipString(i);
      const key = text.slice(i, end);
      let j = end;
      while (j < text.length && isWs(text[j])) j += 1;
      if (text[j] === ':') {
        const frame = stack[stack.length - 1];
        if (frame && frame.object) {
          if (frame.keys.has(key)) return key;
          frame.keys.add(key);
        }
      }
      i = end;
      continue;
    }
    if (ch === '{') {
      stack.push({ object: true, keys: new Set() });
      i += 1;
      continue;
    }
    if (ch === '[') {
      stack.push({ object: false });
      i += 1;
      continue;
    }
    if (ch === '}' || ch === ']') {
      stack.pop();
      i += 1;
      continue;
    }
    i += 1;
  }
  return null;
}

function readJsonStrict(path) {
  const text = readFileSync(path, 'utf8');
  const duplicate = findDuplicateKey(text);
  if (duplicate !== null) {
    throw new Error(`${path}: duplicate JSON key ${duplicate}`);
  }
  return JSON.parse(text);
}

// ------------------------------------------------------------------- verify

function verify(root, options = {}) {
  const problems = [];
  const manifestPath = join(root, MANIFEST);
  if (!existsSync(manifestPath)) {
    return { ok: false, digest: '', files: 0, problems: [`missing: ${MANIFEST} does not exist`] };
  }
  let manifest;
  try {
    manifest = readJsonStrict(manifestPath);
  } catch (error) {
    return { ok: false, digest: '', files: 0, problems: [`unreadable: ${error.message}`] };
  }
  if (manifest.schema !== MANIFEST_SCHEMA) {
    problems.push(`wrong-schema: ${MANIFEST} schema '${manifest.schema}' != '${MANIFEST_SCHEMA}'`);
  }
  if (manifest.generator !== 'faktor-contracts') {
    problems.push(`wrong-generator: ${MANIFEST} generator '${manifest.generator}'`);
  }
  if (manifest.algorithm !== 'sha256') {
    problems.push(`wrong-algorithm: ${MANIFEST} algorithm '${manifest.algorithm}'`);
  }
  if (typeof manifest.digest !== 'string' || !DIGEST_RE.test(manifest.digest)) {
    problems.push(`wrong-digest: ${MANIFEST} digest is not sha256:<64 hex>`);
  }
  const entries = Array.isArray(manifest.files) ? manifest.files : null;
  if (!entries || entries.length === 0) {
    problems.push(`wrong-files: ${MANIFEST} files must be a non-empty array`);
  }
  const listed = new Map();
  const hasher = createHash('sha256');
  if (entries) {
    entries.forEach((entry, index) => {
      if (!entry || typeof entry.path !== 'string') {
        problems.push(`malformed: files[${index}] is not a path entry`);
        return;
      }
      if (!/^docs\/contracts\/[a-z0-9-]+\.json$/.test(entry.path) || entry.path === MANIFEST) {
        problems.push(`malformed: files[${index}] path '${entry.path}' is not a canonical file`);
        return;
      }
      if (listed.has(entry.path)) {
        problems.push(`duplicate: ${entry.path} is listed twice`);
        return;
      }
      listed.set(entry.path, entry);
      if (typeof entry.sha256 !== 'string' || !DIGEST_RE.test(entry.sha256)) {
        problems.push(`malformed: ${entry.path} sha256 is not sha256:<64 hex>`);
        return;
      }
      const full = join(root, entry.path);
      if (!existsSync(full)) {
        problems.push(`missing: ${entry.path} is listed but does not exist`);
        return;
      }
      const bytes = readFileSync(full);
      const actual = `sha256:${sha256Hex(bytes)}`;
      if (actual !== entry.sha256) {
        problems.push(`hash-mismatch: ${entry.path} hashes ${actual}, manifest says ${entry.sha256}`);
        return;
      }
      // The aggregate digest is the same concatenation the Rust generator
      // hashes: path, newline, file bytes, newline, in manifest order.
      hasher.update(entry.path, 'utf8');
      hasher.update('\n');
      hasher.update(bytes);
      hasher.update('\n');
    });
  }
  const digest = `sha256:${hasher.digest('hex')}`;
  if (typeof manifest.digest === 'string' && digest !== manifest.digest) {
    problems.push(`digest-mismatch: recomputed ${digest} != manifest ${manifest.digest}`);
  }
  // The frozen directory must contain exactly the manifest's files.
  const dir = join(root, CONTRACT_DIR);
  const onDisk = existsSync(dir)
    ? readdirSync(dir)
        .filter((name) => name.endsWith('.json') && name !== 'manifest.json')
        .map((name) => `${CONTRACT_DIR}/${name}`)
        .sort()
    : [];
  const declared = [...listed.keys()].sort();
  for (const path of onDisk) {
    if (!declared.includes(path)) {
      problems.push(`unlisted: ${path} exists but is not part of the compiled vocabulary`);
    }
  }
  for (const path of declared) {
    if (!onDisk.includes(path)) {
      problems.push(`missing: ${path} is listed but not on disk`);
    }
  }
  // Every frozen file must parse as strict JSON with the canonical schema.
  if (options.checkSchemas !== false) {
    for (const path of onDisk) {
      try {
        const record = readJsonStrict(join(root, path));
        if (record.schema !== CONTRACT_SCHEMA) {
          problems.push(`wrong-schema: ${path} schema '${record.schema}' != '${CONTRACT_SCHEMA}'`);
        }
      } catch (error) {
        problems.push(`unreadable: ${path}: ${error.message}`);
      }
    }
  }
  return { ok: problems.length === 0, digest, files: listed.size, problems };
}

// ------------------------------------------------------------------ selftest

function runSelftest() {
  const failures = [];
  const scratch = mkdtempSync(join(tmpdir(), 'faktor-contracts-selftest-'));
  const source = join(SCRIPT_ROOT, CONTRACT_DIR);
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
    if (!condition) throw new Error(message);
  };
  const freshCopy = () => {
    const root = mkdtempSync(join(scratch, 'tree-'));
    const target = join(root, CONTRACT_DIR);
    mkdirSync(target, { recursive: true });
    for (const name of readdirSync(source)) {
      writeFileSync(join(target, name), readFileSync(join(source, name)));
    }
    return root;
  };
  const expectFail = (label, mutate, code) => {
    const root = freshCopy();
    try {
      mutate(root);
      const result = verify(root);
      assert(!result.ok, `${label}: expected failure but verification passed`);
      assert(
        result.problems.some((problem) => problem.startsWith(`${code}:`)),
        `${label}: expected a '${code}:' problem, got ${JSON.stringify(result.problems)}`,
      );
      console.log(`selftest ok: ${label}`);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  };
  const editJson = (root, rel, mutate) => {
    const path = join(root, rel);
    const record = readJsonStrict(path);
    mutate(record);
    writeFileSync(path, `${JSON.stringify(record, null, 2)}\n`);
  };

  try {
    run('the checked-in tree verifies and names a digest', () => {
      const result = verify(SCRIPT_ROOT);
      assert(result.ok, `expected pass, problems=${JSON.stringify(result.problems)}`);
      assert(DIGEST_RE.test(result.digest), `bad digest ${result.digest}`);
      assert(result.files >= 8, `expected at least 8 frozen files, got ${result.files}`);
    });
    run('a scratch copy verifies before mutation', () => {
      const root = freshCopy();
      try {
        const result = verify(root);
        assert(result.ok, `fresh copy must verify: ${JSON.stringify(result.problems)}`);
      } finally {
        rmSync(root, { recursive: true, force: true });
      }
    });
    expectFail(
      'a renamed variant fails',
      (root) =>
        editJson(root, `${CONTRACT_DIR}/event-kind.json`, (record) => {
          const variant = record.variants.find((entry) => entry.wire === 'compact_rejected');
          variant.wire = 'compaction_rejected';
        }),
      'hash-mismatch',
    );
    expectFail(
      'a reordered variant list fails',
      (root) =>
        editJson(root, `${CONTRACT_DIR}/error-kind.json`, (record) => {
          const [first, second] = record.variants;
          record.variants[0] = second;
          record.variants[1] = first;
        }),
      'hash-mismatch',
    );
    expectFail(
      'a tampered manifest digest fails',
      (root) =>
        editJson(root, MANIFEST, (record) => {
          record.digest = `sha256:${'0'.repeat(64)}`;
        }),
      'digest-mismatch',
    );
    expectFail(
      'an extra frozen file fails',
      (root) => {
        writeFileSync(
          join(root, CONTRACT_DIR, 'not-a-contract.json'),
          '{"schema":"faktor-frozen-contract/v1"}\n',
        );
      },
      'unlisted',
    );
    expectFail(
      'a missing frozen file fails',
      (root) => {
        rmSync(join(root, CONTRACT_DIR, 'task-state.json'));
      },
      'missing',
    );
    expectFail(
      'a manifest entry for a deleted file fails',
      (root) =>
        editJson(root, MANIFEST, (record) => {
          record.files.pop();
        }),
      'unlisted',
    );
    expectFail(
      'a duplicate manifest JSON key fails',
      (root) => {
        const path = join(root, MANIFEST);
        const text = readFileSync(path, 'utf8');
        writeFileSync(path, text.replace('"schema":', '"schema": "x", "schema":'));
      },
      'unreadable',
    );
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
  if (failures.length > 0) {
    console.error(`selftest: FAIL (${failures.length} failure(s))`);
    return 1;
  }
  console.log('selftest: PASS (rename/reorder/tamper/extra/missing/duplicate-key matrix)');
  return 0;
}

// --------------------------------------------------------------------- main

function main(argv) {
  const args = argv.slice(2);
  const command = args[0] && !args[0].startsWith('--') ? args[0] : 'verify';
  const rest = command === args[0] ? args.slice(1) : args;
  if (args.includes('-h') || args.includes('--help')) {
    usage();
    return 2;
  }
  const rootIndex = rest.indexOf('--root');
  if (rootIndex !== -1 && !rest[rootIndex + 1]) {
    usage();
    return 2;
  }
  const root = rootIndex === -1 ? SCRIPT_ROOT : resolve(rest[rootIndex + 1]);
  if (command === 'verify') {
    const result = verify(root);
    if (!result.ok) {
      for (const problem of result.problems) {
        console.error(`contracts: ${problem}`);
      }
      console.error(`contracts: FAIL (${result.problems.length} problem(s))`);
      return 1;
    }
    if (rest.includes('--json')) {
      console.log(JSON.stringify({ ok: true, digest: result.digest, files: result.files }));
    } else if (rest.includes('--print-digest')) {
      console.log(result.digest);
    } else {
      console.log(`contracts: PASS (${result.files} frozen files, digest ${result.digest})`);
    }
    return 0;
  }
  if (command === 'selftest') {
    return runSelftest();
  }
  if (command === 'check' || command === 'write') {
    const cargo = spawnSync('cargo', ['run', '-p', 'faktor-contracts', '--', command], {
      cwd: root,
      stdio: 'inherit',
    });
    if (cargo.error) {
      console.error(`contracts: cargo is unavailable: ${cargo.error.message}`);
      return 127;
    }
    if (cargo.status !== 0) {
      return cargo.status ?? 1;
    }
    const result = verify(root);
    if (!result.ok) {
      for (const problem of result.problems) {
        console.error(`contracts: ${problem}`);
      }
      return 1;
    }
    console.log(`contracts: PASS (${result.files} frozen files, digest ${result.digest})`);
    return 0;
  }
  usage();
  return 2;
}

process.exit(main(process.argv));
