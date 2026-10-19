#!/usr/bin/env node
// Build the serialized repository relationship graph consumed by the CI
// selector. The graph is produced by the PRODUCT authority
// (`faktor impact graph`, the same `faktor-impact` builder the semantic
// `affected()` provider uses), never by a second manifest heuristic here.
//
//   node scripts/programme/build-impact-graph.mjs [--root DIR] [--out FILE]
//        [--check] [--selftest]
//
// The selector projection contains crate/package dependencies, file
// inclusions, test-target and file-level coverage edges. The full caller
// graph remains available from the product API (`faktor impact graph`).

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  IMPACT_GRAPH_SCHEMA,
  validateImpactGraph,
} from '../../tests/programme/selector.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const DEFAULT_OUT = 'tests/programme/impact-graph.json';

function arg(args, name, fallback = null) {
  const index = args.indexOf(`--${name}`);
  return index >= 0 && index + 1 < args.length ? args[index + 1] : fallback;
}

export function build({ root = ROOT, out = DEFAULT_OUT, target = null } = {}) {
  const destination = resolve(root, target ?? out);
  mkdirSync(dirname(destination), { recursive: true });
  execFileSync(
    'cargo',
    [
      'run',
      '-q',
      '-p',
      'faktor-cli',
      '--',
      'impact',
      'graph',
      '--root',
      root,
      '--selector',
      '--compact',
      '--out',
      destination,
    ],
    { cwd: root, stdio: 'inherit' },
  );
  const text = readFileSync(destination, 'utf8');
  const error = validateImpactGraph(JSON.parse(text), null);
  if (error) throw new Error(`generated graph failed validation: ${error}`);
  return destination;
}

export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`impact graph selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`impact graph selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const base = {
    schema: IMPACT_GRAPH_SCHEMA,
    revision: { commit: null, tree: null, dirty: false },
    workspace: 1,
    files: [],
    nodes: [
      { id: 'crate:a', kind: 'crate', name: 'a', path: 'a', provenance: 'manifest' },
      { id: 'test:t.rs#t@1', kind: 'test', name: 't', path: 't.rs', crate_name: 'a', provenance: 'coverage' },
    ],
    edges: [{ from: 'test:t.rs#t@1', to: 'crate:a', kind: 'tests', provenance: 'coverage' }],
    reverse_edges: [{ from: 'crate:a', to: 'test:t.rs#t@1', kind: 'tests', provenance: 'coverage' }],
    stats: {},
    digest: 'fixture',
  };
  check('the generator schema validates', validateImpactGraph(base, null) === null);
  const unsorted = structuredClone(base);
  unsorted.nodes.reverse();
  check('unsorted nodes are refused', /sorted/.test(validateImpactGraph(unsorted, null) ?? ''));
  const dangling = structuredClone(base);
  dangling.edges[0].to = 'crate:missing';
  check('dangling edges are refused', /unknown node/.test(validateImpactGraph(dangling, null) ?? ''));
  const stale = structuredClone(base);
  stale.revision.commit = 'old';
  check(
    'a stale revision is refused',
    /stale/.test(validateImpactGraph(stale, 'new') ?? ''),
  );
  check('an unsupported schema is refused', /schema/.test(validateImpactGraph({ ...base, schema: 'x' }, null) ?? ''));

  const dir = mkdtempSync(join(tmpdir(), 'faktor-impact-generator-'));
  try {
    const path = join(dir, 'graph.json');
    writeFileSync(path, `${JSON.stringify(base)}\n`);
    check(
      'the selector loader accepts the generated document',
      validateImpactGraph(JSON.parse(readFileSync(path, 'utf8')), null) === null,
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
  if (failures > 0) {
    console.error(`impact graph selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('impact graph selftest: PASS');
  return 0;
}

export function main() {
  const args = process.argv.slice(2);
  if (args.includes('--selftest')) return selftest();
  const root = resolve(arg(args, 'root', ROOT));
  const out = arg(args, 'out', DEFAULT_OUT);
  if (args.includes('--check')) {
    const target = join(tmpdir(), `faktor-impact-check-${process.pid}.json`);
    try {
      build({ root, target });
      const expected = resolve(root, out);
      const generated = readFileSync(target, 'utf8');
      if (!existsSync(expected) || readFileSync(expected, 'utf8') !== generated) {
        console.error(`impact graph: ${out} is stale; run build-impact-graph.mjs`);
        return 1;
      }
      console.log(`impact graph: ${out} is current`);
      return 0;
    } finally {
      rmSync(target, { force: true });
    }
  }
  const destination = build({ root, out });
  console.log(`impact graph: wrote ${destination}`);
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  process.exitCode = main();
}
