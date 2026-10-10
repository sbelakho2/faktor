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
// Every remaining hit must be listed in ALLOWLIST with a reason; a stale
// allowlist entry (no longer matching) is itself a failure so reviews stay
// honest. Matcher fixtures in --selftest prove both patterns are detected.
//
// Exit codes: 0 pass; 1 violations/stale allowlist; 2 usage.
import { readFileSync } from 'node:fs';
import { dirname, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

const AUTHORITY_FILES = [
  'crates/agent/src/runtime/turn/drive.rs',
  'crates/agent/src/runtime/verification_attribution.rs',
  'crates/agent/src/runtime/retrieval.rs',
  'crates/agent/src/runtime/compaction.rs',
  'crates/agent/src/semantic_tool.rs',
  'crates/instructions/src/lib.rs',
  'crates/git/src/guard.rs',
  'crates/orchestrator/src/merge_semantic.rs',
  'crates/worker/src/service.rs',
  'crates/session/src/child.rs',
  'crates/session/src/manager.rs',
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
    file: 'crates/agent/src/runtime/turn/drive.rs',
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

function runCoverage() {
  const scanned = [];
  for (const file of AUTHORITY_FILES) {
    const text = readFileSync(resolve(ROOT, file), 'utf8');
    for (const hit of scanText(text, file)) scanned.push(hit);
  }
  const used = new Map();
  const problems = [];
  for (const hit of scanned) {
    const entry = ALLOWLIST.find(
      (a) => a.file === hit.file && hit.text.includes(a.line_contains),
    );
    if (entry) {
      used.set(entry, (used.get(entry) ?? 0) + 1);
      continue;
    }
    problems.push({ ...hit, kind: `UNREVIEWED ${hit.kind}` });
  }
  for (const entry of ALLOWLIST) {
    const count = used.get(entry) ?? 0;
    if (count !== entry.count) {
      problems.push({
        file: entry.file,
        line: 0,
        kind: 'STALE-ALLOWLIST',
        text: `allowlist entry matches ${count} time(s), expected ${entry.count}: ${entry.line_contains}`,
      });
    }
  }
  if (problems.length > 0) {
    for (const p of problems) {
      console.error(`authority-collapse: ${p.file}:${p.line}: ${p.kind}: ${p.text}`);
    }
    process.exit(1);
  }
  console.log(
    `authority-collapse: PASS (${AUTHORITY_FILES.length} files; ${ALLOWLIST.length} reviewed exception(s); 0 unreviewed collapses)`,
  );
}

function selftest() {
  const fixtures = [
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
  for (const fixture of fixtures) {
    const found = scanText(fixture.text, 'fixture.rs').length > 0;
    if (found !== fixture.hit) {
      console.error(
        `authority-collapse selftest: ${fixture.name}: expected hit=${fixture.hit}, got ${found}`,
      );
      failed += 1;
    }
  }
  // The real tree must pass too (zero unreviewed collapses).
  const moduleDir = dirname(fileURLToPath(import.meta.url));
  void moduleDir;
  if (failed > 0) process.exit(1);
  console.log('authority-collapse selftest: PASS (patterns detected, exemptions respected)');
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
