#!/usr/bin/env node
// Authority-collapse static check (closure plan item: "a mechanical
// authority-collapse checker with explicit allowlist").
//
// The six laws say: an UNREADABLE authority read (store failure, decode
// error, I/O error) may never silently become absence/default/ALL. This
// checker rejects the collapse PATTERNS in the authority-bearing modules:
//
//   1. `.ok()` / `.unwrap_or_default()` / `.unwrap_or(...)` on a call whose
//      receiver looks like an authority source (handle/store/plane/session
//      reads, memory facts, task rows, checkpoints, tokens, workers,
//      leases, the instruction resolver);
//   2. an `Err(_)` arm whose body is bare `None`/`{}`/empty (a swallowed
//      infrastructure error) in the same modules.
//
// DISCOVERY IS MECHANICAL — there is no manual file array:
//
//   * every production Rust file under `crates/*/src/**` (tests excluded)
//     that carries the required module annotation
//     `@authority-collapse: authority-bearing` is an authority module and is
//     scanned. New authority modules enter coverage automatically by
//     carrying the annotation; the verification/contract/semantic surfaces
//     (contract_eval, crates/verify, crates/kiwi, crates/semantic) are
//     annotated, so they can never silently leave coverage;
//   * a production file that DECLARES an authority surface (verification
//     provider trait/registry, claim-batch dispatch, contract evaluation/
//     outcome functions, semantic provider trait/registry) but does NOT
//     carry the annotation is itself a failure — an unannotated authority
//     module is a discovered-but-unreviewed hole.
//
// Every remaining hit must be listed in ALLOWLIST with a reason; a stale
// allowlist entry (no longer matching) is itself a failure so reviews stay
// honest. The --selftest fixtures prove both collapse patterns are detected,
// that a NEW annotated authority file is discovered by the scan alone, and
// that an unannotated authority file fails discovery.
//
// Exit codes: 0 pass; 1 violations/stale allowlist/unannotated authority;
// 2 usage.
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

// The required annotation of every authority-bearing production module.
const AUTHORITY_ANNOTATION = /@authority-collapse:\s*authority-bearing/;

// Authority-SURFACE declarations: a production module that defines or
// dispatches one of these must carry the annotation (or discovery refuses
// it as unannotated). This is a pattern list, not a file list: a new
// provider/registry/dispatch module is caught the day it lands.
const REQUIRED_ANNOTATION_PATTERNS = [
  /trait\s+VerificationProvider\b/,
  /struct\s+VerificationRegistry\b/,
  /impl\s+VerificationProvider\s+for\b/,
  /\bfn\s+verify_claims\b/,
  /\bfn\s+evaluate_and_persist_contract\b/,
  /\bfn\s+apply_contract_outcome\b/,
  /\bfn\s+set_verification_providers\b/,
  /trait\s+SemanticProvider\b/,
  /struct\s+SemanticProviderRegistry\b/,
  /impl\s+SemanticProviderRegistry\b/,
];

// Reviewed exceptions. `line_contains` is a literal substring of the source
// line; `count` must match exactly (changes force re-review).
const ALLOWLIST = [
  {
    file: 'crates/agent/src/runtime/turn/drive.rs',
    line_contains: 'handle.turn_cancellation(admitted.op_id).unwrap_or_default()',
    count: 1,
    reason:
      'cancellation-parent lookup for an ADMITTED operation: absence of a parent token is a legal root state (not a store failure).',
  },
  {
    file: 'crates/agent/src/runtime/turn/drive_phases.rs',
    line_contains: 'handle.title().unwrap_or_default()',
    count: 1,
    reason: 'prompt hint title: display-only context, never an authority or identity input.',
  },
  {
    file: 'crates/agent/src/runtime/retrieval.rs',
    line_contains: 'handle.title().unwrap_or_default()',
    count: 1,
    reason: 'prompt hint title: display-only context, never an authority or identity input.',
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: 'let mut tasks = handle.list_tasks().unwrap_or_default();',
    count: 1,
    reason:
      'review-context support read: an unreadable durable task list yields no durable criteria; the caller still carries criteria_rows and the goal text, so the reviewer never receives a silently empty package when a goal exists (this helper never gates completion).',
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: 'let preferred = handle.task_id().ok();',
    count: 1,
    reason:
      "advisory task preference for the review package: an unreadable task id falls back to the session's first task; the value never gates completion, rebinds a task or feeds a proof.",
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: '.unwrap_or_default()',
    count: 1,
    reason:
      'Option::unwrap_or_default after a positive position selection on a non-empty task list: unreachable default kept for the total function shape.',
  },
  {
    file: 'crates/agent/src/runtime/turn/state.rs',
    line_contains: 'let last = handle.last_event_seq().ok()??;',
    count: 1,
    reason:
      'last_event_kind diagnostic: an unreadable event sequence is explicit absence (None), never a fabricated event, and the value only classifies a display/telemetry decision.',
  },
  {
    file: 'crates/agent/src/runtime/verification_attribution.rs',
    line_contains: 'cas.get_bounded(h, REVIEW_SIDE_BOUND).ok().flatten()',
    count: 1,
    reason:
      'review after-side fallback chain: a CAS read failure falls through to the workspace read, which now REFUSES typed on failure — no silent absence can result.',
  },
  {
    file: 'crates/agent/src/runtime/verification_attribution.rs',
    line_contains: 'let row = handle.row().ok()?;',
    count: 1,
    reason:
      'fingerprint_workspace auxiliary observation (documented): no manifest observation degrades the ENVIRONMENT projection; the reusable proof basis refuses separately when the content identity is unavailable.',
  },
  {
    file: 'crates/agent/src/runtime/verification_attribution.rs',
    line_contains: '.ok()??;',
    count: 1,
    reason:
      'fingerprint_workspace root read (same auxiliary function): an unreadable root yields NO observation, never a fabricated one; the proof basis refuses separately when identity is required.',
  },
  {
    file: 'crates/agent/src/runtime/verification_attribution.rs',
    line_contains: 'workspaces.open(row.workspace_id, root).ok()',
    count: 1,
    reason:
      'fingerprint_workspace workspace open (same auxiliary function): an unopenable workspace yields no manifest observation; the proof basis refuses separately.',
  },
  {
    file: 'crates/agent/src/runtime/verification_attribution.rs',
    line_contains: '.unwrap_or_default()',
    count: 1,
    reason:
      'Option::unwrap_or_default AFTER a typed read (`handle.get_task(task_id)?`): a missing task row is legal absence (empty criteria), and the read error already propagated with `?`.',
  },
  {
    file: 'crates/agent/src/runtime/compaction.rs',
    line_contains: 'let Ok(row) = handle.row() else',
    count: 1,
    reason:
      'semantic consult on an unreadable session row returns SemanticTurnState::unknown (fail-closed escalation), never absence — the else branch is the conservative Unknown, not a default.',
  },
  {
    file: 'crates/agent/src/runtime/compaction.rs',
    line_contains: '.ok()',
    count: 1,
    reason:
      'workspace-root resolve inside the consult: the failure flows into the explicit revision-unavailable branch that degrades to Unknown with a warn — never a fabricated snapshot identity.',
  },
  {
    file: 'crates/agent/src/runtime/retrieval.rs',
    line_contains: 'let Ok(identity) = handle.identity() else',
    count: 1,
    reason:
      'evidence-poll archive on an unreadable identity: the else branch logs a typed warn and SKIPS the archive; no guessed workspace id, no fabricated identity.',
  },
];

// (1) collapse-on-authority-call
const AUTHORITY_RECEIVER =
  /(?:handle|store|plane|session)\.[a-z_]+\(|\.memory_facts\(|\.list_[a-z_]*\(|\.get_[a-z_]*\(|\.task_id\(|\.row\(|\.checkpoints\(|\.token\(|\.worker\(|\.lease[a-z_]*\(|resolver\.resolve\(/;
// `.unwrap_or_else`/`.map_or` fabricate defaults exactly like unwrap_or.
const COLLAPSE = /\.ok\(\)|\.unwrap_or_default\(\)|\.unwrap_or\(|\.unwrap_or_else\(|\.map_or\(/;
// (2) bare err-arm: `Err(_) => None,` / `Err(_) => {}` / `Err(_) => { }` /
//     `Err(_) => return <default>;` — fabricating a default in an authority
//     path is the same failure mode as a bare None.
const BARE_ERR_ARM =
  /Err\(_\s*\)\s*=>\s*(?:\{\s*\}|\{\s*return\s+None;?\s*\}|None\s*,?|\{\s*return\s+(?:Ok\(None\)|Vec::new\(\)|String::new\(\)|false);?\s*\}|return\s+(?:Ok\(None\)|Vec::new\(\)|String::new\(\)|false);?)\s*$/;
// (3) `let Ok(`/`let Some` on an authority read: a value pattern on a
//     Result is a collapse unless the else branch refuses typed (the
//     allowlist/reviewer confirms the else shape).
const LET_OK = /let\s+Ok\(/;

function scanText(text, file) {
  const hits = [];
  const lines = text.split('\n');
  lines.forEach((line, index) => {
    const trimmed = line.trim();
    if (trimmed.startsWith('//')) return;
    // Multi-line receivers: a collapse continuation on this line whose
    // previous two lines reference an authority source.
    const window = `${lines[index - 2] ?? ''} ${lines[index - 1] ?? ''} ${line}`;
    if (COLLAPSE.test(line) && (AUTHORITY_RECEIVER.test(line) || AUTHORITY_RECEIVER.test(window))) {
      hits.push({ file, line: index + 1, kind: 'collapse', text: trimmed });
    }
    if (BARE_ERR_ARM.test(trimmed)) {
      hits.push({ file, line: index + 1, kind: 'bare-err-arm', text: trimmed });
    }
    if (LET_OK.test(line) && AUTHORITY_RECEIVER.test(line)) {
      hits.push({ file, line: index + 1, kind: 'let-ok', text: trimmed });
    }
  });
  return hits;
}

// ------------------------------------------------- mechanical discovery

function isTestPath(rel) {
  const segments = rel.split('/');
  return (
    rel.endsWith('_tests.rs') ||
    rel.endsWith('/tests.rs') ||
    segments.includes('tests') ||
    segments.includes('test')
  );
}

function walkProductionRust(dir, out) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const entry of entries) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      walkProductionRust(path, out);
    } else if (entry.name.endsWith('.rs')) {
      out.push(relative(ROOT, path));
    }
  }
}

function productionFiles() {
  const cratesDir = resolve(ROOT, 'crates');
  const found = [];
  for (const entry of readdirSync(cratesDir, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    walkProductionRust(join(cratesDir, entry.name, 'src'), found);
  }
  return found;
}

/**
 * Mechanical authority discovery over a list of { path, text } production
 * files: annotated modules are covered; surface-declaring modules without
 * the annotation are refused. No file list exists anywhere.
 */
function discoverAuthorityFiles(files) {
  const covered = [];
  const unannotated = [];
  for (const file of files) {
    if (isTestPath(file.path)) continue;
    const annotated = AUTHORITY_ANNOTATION.test(file.text);
    if (annotated) {
      covered.push(file.path);
      continue;
    }
    if (REQUIRED_ANNOTATION_PATTERNS.some((pattern) => pattern.test(file.text))) {
      unannotated.push(file.path);
    }
  }
  return { covered, unannotated };
}

function readTree(files) {
  return files.map((path) => ({ path, text: readFileSync(resolve(ROOT, path), 'utf8') }));
}

// --------------------------------------------------------------- coverage

function runCoverage() {
  const tree = readTree(productionFiles());
  const { covered, unannotated } = discoverAuthorityFiles(tree);
  const problems = [];
  let scanned = 0;
  for (const path of covered) {
    const { text } = tree.find((file) => file.path === path);
    for (const hit of scanText(text, path)) scanned += 1, problems.push(hit);
  }
  for (const path of unannotated) {
    console.error(
      `authority-collapse: ${path}: UNANNOTATED-AUTHORITY: file declares an authority surface without the //! @authority-collapse: authority-bearing annotation`,
    );
  }
  const used = new Map();
  const unreviewed = [];
  for (const hit of problems) {
    const entry = ALLOWLIST.find(
      (a) => a.file === hit.file && hit.text.includes(a.line_contains),
    );
    if (entry) {
      used.set(entry, (used.get(entry) ?? 0) + 1);
      continue;
    }
    unreviewed.push({ ...hit, kind: `UNREVIEWED ${hit.kind}` });
  }
  for (const entry of ALLOWLIST) {
    const count = used.get(entry) ?? 0;
    if (count !== entry.count) {
      unreviewed.push({
        file: entry.file,
        line: 0,
        kind: 'STALE-ALLOWLIST',
        text: `allowlist entry matches ${count} time(s), expected ${entry.count}: ${entry.line_contains}`,
      });
    }
  }
  for (const p of unreviewed) {
    console.error(`authority-collapse: ${p.file}:${p.line}: ${p.kind}: ${p.text}`);
  }
  if (unannotated.length > 0 || unreviewed.length > 0) {
    process.exit(1);
  }
  console.log(
    `authority-collapse: PASS (${covered.length} authority module(s) discovered mechanically of ${tree.length} production file(s); ${ALLOWLIST.length} reviewed exception(s); 0 unreviewed collapses)`,
  );
}

// --------------------------------------------------------------- selftest

function selftest() {
  const patternFixtures = [
    {
      name: 'collapse on a task read',
      hit: true,
      text: '        let task = handle.get_task(task_id).ok().flatten();',
    },
    {
      name: 'unwrap_or_default on memory facts',
      hit: true,
      text: '        let facts = handle.memory_facts().unwrap_or_default();',
    },
    {
      name: 'bare Err(_) arm',
      hit: true,
      text: '            Err(_) => None,',
    },
    {
      name: 'typed refusal is NOT a violation',
      hit: false,
      text: '            Err(e) => return Err(ExecError::SemanticRequired(format!("{e}"))),',
    },
    {
      name: 'non-authority JSON serialization is NOT a violation',
      hit: false,
      text: '        let spec = serde_json::to_string(spec).unwrap_or_default();',
    },
    {
      name: 'unwrap_or_else fabricating a workspace is a violation',
      hit: true,
      text: '        let ws = handle.identity().map(|i| i.workspace_id).unwrap_or_else(|_| WorkspaceId::new(1));',
    },
    {
      name: 'multi-line receiver collapse continuation is a violation',
      hit: true,
      text: [
        '        let project_key = self',
        '            .deps',
        '            .session',
        '            .resolve_workspace_root(handle.id())',
        '            .ok()',
      ].join('\n'),
    },
    {
      name: 'let Ok on an authority read is reviewed, not auto-passed',
      hit: true,
      text: '        let Ok(identity) = handle.identity() else {',
    },
    {
      name: 'Err(_) return-default arm is a violation',
      hit: true,
      text: '            Err(_) => return Ok(None);',
    },
  ];
  let failed = 0;
  for (const fixture of patternFixtures) {
    const found = scanText(fixture.text, 'fixture.rs').length > 0;
    if (found !== fixture.hit) {
      console.error(
        `authority-collapse selftest: ${fixture.name}: expected hit=${fixture.hit}, got ${found}`,
      );
      failed += 1;
    }
  }

  // Discovery fixtures: a NEW authority file is found by the scan alone
  // (annotation registry), and an unannotated authority surface is refused.
  const newAuthorityWithAnnotation = {
    path: 'crates/agent/src/runtime/brand_new_authority.rs',
    text: [
      '//! A brand-new authority module added after the checker shipped.',
      '//! @authority-collapse: authority-bearing',
      'impl VerificationProvider for NewProvider {',
      '    fn ask(&self) {',
      '        let task = handle.get_task(id).ok();',
      '    }',
      '}',
    ].join('\n'),
  };
  const newAuthorityUnannotated = {
    path: 'crates/agent/src/runtime/brand_new_authority.rs',
    text: [
      'impl VerificationProvider for NewProvider {',
      '    fn ask(&self) {',
      '        let task = handle.get_task(id).ok();',
      '    }',
      '}',
    ].join('\n'),
  };
  const annotatedWithoutSurfacePatterns = {
    path: 'crates/agent/src/runtime/annotated_only.rs',
    text: [
      '//! @authority-collapse: authority-bearing',
      'pub fn peek() {',
      '    let facts = handle.memory_facts().unwrap_or_default();',
      '}',
    ].join('\n'),
  };
  const plainModule = {
    path: 'crates/util/src/helpers.rs',
    text: 'pub fn add(a, b) {\n    a + b\n}\n',
  };
  const discoveryCases = [
    {
      name: 'a NEW annotated authority file is discovered and scanned',
      files: [newAuthorityWithAnnotation],
      covered: 1,
      unannotated: 0,
      hits: 1,
    },
    {
      name: 'an unannotated authority file fails discovery',
      files: [newAuthorityUnannotated],
      covered: 0,
      unannotated: 1,
      hits: 0,
    },
    {
      name: 'an annotated module is covered even without surface declarations',
      files: [annotatedWithoutSurfacePatterns],
      covered: 1,
      unannotated: 0,
      hits: 1,
    },
    {
      name: 'a plain module without authority patterns is not required to annotate',
      files: [plainModule],
      covered: 0,
      unannotated: 0,
      hits: 0,
    },
    {
      name: 'test files are excluded from discovery',
      files: [
        {
          path: 'crates/agent/src/runtime/authority_tests.rs',
          text: 'impl VerificationProvider for TestProvider {}\n',
        },
      ],
      covered: 0,
      unannotated: 0,
      hits: 0,
    },
  ];
  for (const fixture of discoveryCases) {
    const { covered, unannotated } = discoverAuthorityFiles(fixture.files);
    const hits = covered.reduce(
      (total, path) =>
        total + scanText(fixture.files.find((f) => f.path === path).text, path).length,
      0,
    );
    if (
      covered.length !== fixture.covered ||
      unannotated.length !== fixture.unannotated ||
      hits !== fixture.hits
    ) {
      console.error(
        `authority-collapse selftest: ${fixture.name}: expected covered=${fixture.covered} unannotated=${fixture.unannotated} hits=${fixture.hits}, got covered=${covered.length} unannotated=${unannotated.length} hits=${hits}`,
      );
      failed += 1;
    }
  }

  if (failed > 0) process.exit(1);
  console.log(
    'authority-collapse selftest: PASS (patterns detected, mechanical discovery finds new authority files, unannotated authority refused)',
  );
}

const mode = process.argv[2] ?? '--coverage';
if (mode === '--selftest') {
  selftest();
} else if (mode === '--coverage') {
  runCoverage();
} else if (mode === '--selftest-then-coverage') {
  selftest();
  runCoverage();
} else {
  console.error(`usage: node ${relative(ROOT, fileURLToPath(import.meta.url))} [--coverage|--selftest]`);
  process.exit(2);
}
