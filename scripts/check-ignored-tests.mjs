#!/usr/bin/env node
// Ignored-test inventory gate (Phase E item 23).
//
// Every `#[ignore]`d test in the workspace must:
//   1. follow the documented tag convention
//      `[fault] [soak] [security] [release] [perf] [live-paid]` (plus the
//      AGENTS.md `[visual]` baseline category), and
//   2. be either assigned to a known lane (`fault` / `release` / `soak`
//      nightly lanes, plus the `perf` and `live-paid` lanes documented in
//      `scripts/certification/ignored-tests.json`), or explicitly documented
//      as manual/live-paid with a reason, or registered as a non-test helper
//      (the Windows re-exec helper is the only such case).
//
// "Assigned to a lane" is checked executably, not declaratively: the lane's
// step must exist in `.woodpecker/trusted/<workflow>.yaml`, its recorded
// command must match the step verbatim, and the command must select the
// package that owns the test file. A test can therefore not be claimed as
// covered by a lane that never compiles it.
//
// Both drift directions fail: a discovered ignored test with no inventory
// entry is `unassigned`, and an inventory entry whose test no longer exists
// is `stale-registry`.
//
// Usage:
//   node scripts/check-ignored-tests.mjs [--root DIR] [--registry FILE]
//   node scripts/check-ignored-tests.mjs --print-inventory
//   node scripts/check-ignored-tests.mjs --selftest
//   node scripts/check-ignored-tests.mjs --root FIXTURE --registry FIXTURE.json --expect-fail CODE
//
// Exit codes: 0 pass (or expected failure matched); 1 violations; 2 usage.

import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SCRIPT_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function argValue(args, name, fallback = '') {
  const idx = args.indexOf(name);
  if (idx === -1) return fallback;
  const value = args[idx + 1];
  if (value === undefined || value.startsWith('--')) return '';
  return value;
}

function walk(dir, out = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'target' || entry.name === 'node_modules' || entry.name.startsWith('.')) {
      continue;
    }
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      walk(full, out);
    } else if (entry.isFile() && entry.name.endsWith('.rs')) {
      out.push(full);
    }
  }
  return out;
}

function discoverSites(root) {
  const sites = [];
  const seen = new Set();
  for (const top of ['crates', 'tests']) {
    const dir = join(root, top);
    if (!existsSync(dir)) continue;
    for (const file of walk(dir)) {
      const lines = readFileSync(file, 'utf8').split('\n');
      for (let i = 0; i < lines.length; i += 1) {
        const match = /^\s*#\[ignore(?:\s*=\s*"((?:[^"\\]|\\.)*)")?\]/.exec(lines[i]);
        if (!match) continue;
        let fn = null;
        for (let j = i + 1; j < Math.min(i + 12, lines.length); j += 1) {
          const fm = /^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)/.exec(lines[j]);
          if (fm) {
            fn = fm[1];
            break;
          }
        }
        const rel = relative(root, file).split('\\').join('/');
        const key = `${rel}#${fn || `line${i + 1}`}`;
        if (seen.has(key)) continue;
        seen.add(key);
        const reason = match[1] || '';
        const tagMatch = /^\[([a-z-]+)\]/.exec(reason);
        sites.push({ key, file: rel, fn, line: i + 1, reason, tag: tagMatch ? tagMatch[1] : null });
      }
    }
  }
  return sites;
}

function packageOf(root, relFile) {
  let dir = dirname(join(root, relFile));
  while (dir.startsWith(root) && dir !== root) {
    const cargo = join(dir, 'Cargo.toml');
    if (existsSync(cargo)) {
      const text = readFileSync(cargo, 'utf8');
      const pkg = /\[package\][\s\S]*?name\s*=\s*"([^"]+)"/.exec(text);
      if (pkg) return pkg[1];
    }
    dir = dirname(dir);
  }
  return null;
}

// Collects the command items of one `- name:` step (same shape as
// scripts/certification/evidence.mjs so drift is judged identically).
function laneCommands(yamlText, step) {
  const lines = yamlText.split('\n');
  let inStep = false;
  let inCommands = false;
  let blockIndent = null;
  let current = null;
  const items = [];
  for (const line of lines) {
    const stepMatch = /^  - name:\s*(\S+)\s*$/.exec(line);
    if (stepMatch) {
      inStep = stepMatch[1] === step;
      inCommands = false;
      blockIndent = null;
      current = null;
      continue;
    }
    if (!inStep) continue;
    if (/^    commands:\s*$/.test(line)) {
      inCommands = true;
      continue;
    }
    if (!inCommands) continue;
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
  return items.flat().map((raw) => raw.trim()).filter((raw) => raw !== '');
}

function runCheck({ root, registryPath }) {
  const problems = [];
  const add = (code, detail) => problems.push(`${code}: ${detail}`);

  if (!existsSync(registryPath)) {
    add('missing-registry', `${relative(SCRIPT_ROOT, registryPath)} does not exist`);
    return { problems, sites: [] };
  }
  let registry;
  try {
    registry = JSON.parse(readFileSync(registryPath, 'utf8'));
  } catch (error) {
    add('bad-registry', `${relative(SCRIPT_ROOT, registryPath)} is not valid JSON: ${error.message}`);
    return { problems, sites: [] };
  }
  if (registry.schema !== 'faktor-ignored-tests/v1') {
    add('bad-registry', `schema '${registry.schema}' != 'faktor-ignored-tests/v1'`);
  }
  const allowedTags = new Set(registry.allowed_tags || []);
  const lanes = registry.lanes || {};
  const requiredLanes = registry.required_nightly_lanes || [];

  // Lane executability: step exists in the workflow YAML and its command
  // matches the recorded one verbatim.
  const workflowCache = new Map();
  const wiredCommands = new Map();
  for (const [name, lane] of Object.entries(lanes)) {
    const file = join(root, '.woodpecker/trusted', lane.workflow === 'nightly' ? 'nightly.yaml' : 'trusted.yaml');
    if (!existsSync(file)) {
      add('lane-unwired', `lane '${name}' workflow file ${relative(root, file)} does not exist`);
      continue;
    }
    if (!workflowCache.has(file)) workflowCache.set(file, readFileSync(file, 'utf8'));
    const commands = laneCommands(workflowCache.get(file), lane.step);
    wiredCommands.set(name, commands);
    if (!commands.includes(lane.command)) {
      add(
        'lane-unwired',
        `lane '${name}' step '${lane.step}' in ${relative(root, file)} does not run the recorded command ` +
          `'${lane.command}' (found: ${JSON.stringify(commands)})`,
      );
    }
  }
  for (const name of requiredLanes) {
    const lane = lanes[name];
    if (!lane) {
      add('lane-unwired', `required nightly lane '${name}' is not declared`);
    } else if (lane.workflow !== 'nightly') {
      add('lane-unwired', `required nightly lane '${name}' is assigned to workflow '${lane.workflow}'`);
    }
  }

  const sites = discoverSites(root);
  const assignmentByKey = new Map();
  const manualByKey = new Map();
  const nonTestByKey = new Map();
  const register = (map, entry, kind) => {
    const key = `${entry.file}#${entry.fn}`;
    if (map.has(key)) add('duplicate-entry', `${kind} entry '${key}' appears twice`);
    map.set(key, entry);
  };
  for (const entry of registry.assignments || []) register(assignmentByKey, entry, 'assignment');
  for (const entry of registry.manual || []) register(manualByKey, entry, 'manual');
  for (const entry of registry.non_test || []) register(nonTestByKey, entry, 'non_test');

  const siteKeys = new Set(sites.map((site) => site.key));
  for (const site of sites) {
    if (nonTestByKey.has(site.key)) {
      continue;
    }
    const assignment = assignmentByKey.get(site.key);
    const manual = manualByKey.get(site.key);
    if (!assignment && !manual) {
      if (!site.tag || !allowedTags.has(site.tag)) {
        add(
          'unconventional-tag',
          `${site.file}#${site.fn || `line${site.line}`} ignore reason '${site.reason}' must start with one of ` +
            `${[...allowedTags].map((t) => `[${t}]`).join(' ')} or be registered under non_test/manual`,
        );
      } else {
        add(
          'unassigned',
          `${site.file}#${site.fn || `line${site.line}`} is [${site.tag}] but has no lane assignment and no documented manual/live-paid reason`,
        );
      }
      continue;
    }
    if (manual) {
      if (!site.tag || !allowedTags.has(site.tag)) {
        add('unconventional-tag', `${site.file}#${site.fn} ignore reason '${site.reason}' has no conventional tag`);
      }
      if (manual.tag && site.tag !== manual.tag) {
        add('tag-drift', `${site.file}#${site.fn} discovered tag '[${site.tag}]' != registry tag '[${manual.tag}]'`);
      }
      if (!manual.reason || !String(manual.reason).trim()) {
        add('missing-reason', `${site.file}#${site.fn} manual entry needs a reason`);
      }
      continue;
    }
    const lane = lanes[assignment.lane];
    if (!lane) {
      add('unknown-lane', `${site.file}#${site.fn} assigned to unknown lane '${assignment.lane}'`);
      continue;
    }
    if (!site.tag || !allowedTags.has(site.tag)) {
      add('unconventional-tag', `${site.file}#${site.fn} ignore reason '${site.reason}' has no conventional tag`);
    }
    const pkg = packageOf(root, site.file);
    if (!pkg) {
      add('package-unknown', `${site.file}#${site.fn} has no enclosing Cargo.toml package`);
    } else if (!(wiredCommands.get(assignment.lane) || []).some((cmd) => cmd.includes(`-p ${pkg}`))) {
      add(
        'package-mismatch',
        `lane '${assignment.lane}' never selects package '${pkg}' (owner of ${site.file}#${site.fn})`,
      );
    }
  }

  for (const [kind, map] of [
    ['assignment', assignmentByKey],
    ['manual', manualByKey],
    ['non_test', nonTestByKey],
  ]) {
    for (const key of map.keys()) {
      if (!siteKeys.has(key)) {
        add('stale-registry', `${kind} entry '${key}' matches no discovered #[ignore] site`);
      }
    }
  }

  return { problems, sites };
}

function printInventory(root) {
  for (const site of discoverSites(root).sort((a, b) => a.key.localeCompare(b.key))) {
    const pkg = packageOf(root, site.file) || '?';
    console.log(`${site.file}\t${site.fn || `line${site.line}`}\t${site.tag || 'NO-TAG'}\t${pkg}`);
  }
}

function runSelftest() {
  const fixtures = join(SCRIPT_ROOT, 'scripts/certification/fixtures/ignored-tests');
  const cases = [
    { name: 'clean', expect: null },
    { name: 'unassigned', expect: 'unassigned' },
    { name: 'bad-tag', expect: 'unconventional-tag' },
  ];
  let failures = 0;
  for (const testCase of cases) {
    const root = join(fixtures, testCase.name);
    const registryPath = join(root, 'ignored-tests.json');
    const { problems } = runCheck({ root, registryPath });
    if (testCase.expect === null) {
      if (problems.length === 0) {
        console.log(`selftest ok: fixture '${testCase.name}' passes`);
      } else {
        console.error(`selftest FAIL: fixture '${testCase.name}' should pass, got: ${problems.join('; ')}`);
        failures += 1;
      }
    } else if (problems.some((problem) => problem.startsWith(`${testCase.expect}:`))) {
      console.log(`selftest ok: planted violation '${testCase.name}' fails with ${testCase.expect}`);
    } else {
      console.error(
        `selftest FAIL: fixture '${testCase.name}' should fail with '${testCase.expect}', got: ${problems.join('; ') || '(none)'}`,
      );
      failures += 1;
    }
  }
  if (failures > 0) {
    console.error(`check-ignored-tests selftest: FAIL (${failures} case(s))`);
    return 1;
  }
  console.log('check-ignored-tests selftest: PASS (planted unassigned/tag violations rejected)');
  return 0;
}

function main() {
  const args = process.argv.slice(2);
  if (args.includes('-h') || args.includes('--help')) {
    console.log(
      'usage: node scripts/check-ignored-tests.mjs [--root DIR] [--registry FILE] [--print-inventory] [--selftest] [--expect-fail CODE]',
    );
    return 2;
  }
  if (args.includes('--selftest')) {
    return runSelftest();
  }
  const root = resolve(argValue(args, '--root', SCRIPT_ROOT));
  if (args.includes('--print-inventory')) {
    printInventory(root);
    return 0;
  }
  const registryPath = resolve(argValue(args, '--registry', join(root, 'scripts/certification/ignored-tests.json')));
  const { problems, sites } = runCheck({ root, registryPath });
  const expectFail = argValue(args, '--expect-fail');
  if (expectFail) {
    if (problems.some((problem) => problem.startsWith(`${expectFail}:`))) {
      console.log(`check-ignored-tests: expected failure '${expectFail}' reproduced (${problems.length} problem(s))`);
      return 0;
    }
    console.error(`check-ignored-tests: expected '${expectFail}:' but got ${JSON.stringify(problems)}`);
    return 1;
  }
  if (problems.length > 0) {
    for (const problem of problems) console.error(`ignored-tests: ${problem}`);
    console.error(`check-ignored-tests: FAIL (${problems.length} problem(s) over ${sites.length} #[ignore] site(s))`);
    return 1;
  }
  console.log(`check-ignored-tests: PASS (${sites.length} #[ignore] site(s) tagged and lane-assigned or documented)`);
  return 0;
}

process.exit(main());
