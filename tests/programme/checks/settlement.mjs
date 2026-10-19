#!/usr/bin/env node
// Static ban on lazy task_id() reads in settlement code (Report.pdf row 1).
//
// A settlement tail is admitted under ONE task identity. Re-reading the
// session's (possibly advanced) identity inside the tail folds the old turn's
// goal/spend/completion gate into the next task row. The fix threads the
// admitted TaskId through `*_bound` calls; this scan fails when a settlement
// write path reads the identity lazily again.
//
// Ratchet: the reviewed baseline below may only shrink. Deleting an allowed
// accessor is fine; adding a new lazy read (or moving a settlement mutation
// back to the lazy helper) fails.

import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..', '..');

// Admission-time reads only. Each is drive/session admission or the explicit
// accessor; settlement writes must use the admitted TaskId. Any other lazy
// read in a banned file is a violation (the ratchet may shrink, never grow).
export const SETTLEMENT_ALLOWLIST = Object.freeze({
  'crates/agent/src/runtime/settlement.rs': [
    { function: 'seed_task_budget', reason: 'drive-start admission seeds the durable budget row' },
    { function: 'session_task', reason: 'explicit lazy accessor; settlement callers must use session_task_bound' },
    { function: 'restore_task_rows', reason: 'drive-start reconstruction creates the first row' },
    { function: 'apply_gate_to_task_row', reason: 'cfg(test) shim delegating to the bound implementation' },
  ],
  'crates/agent/src/runtime/turn/drive.rs': [
    { function: 'typed_ledger_drive_start', reason: 'admission reads the session identity once before the tail' },
  ],
  'crates/orchestrator/src/runtime.rs': [
    { function: 'derive_child_projection_from', reason: 'read-only child presentation projection reads the row it already applies; never decides scheduling or money' },
    { function: 'put_plan_row', reason: 'admission-time plan-row seed before any turn; the id is persisted into the row settlement later reads' },
  ],
  'crates/orchestrator/src/task_executor/settlement.rs': [],
  'crates/orchestrator/src/task_executor/recovery.rs': [],
  'crates/orchestrator/src/task_executor/integration.rs': [],
  'crates/agent/src/runtime/turn/state.rs': [
    { function: 'review_durable_criteria', reason: 'read-only review package selection; no task row is written' },
    { function: 'seed_task_attachments', reason: 'turn admission seeds the attachment plan before the completion gate reads the row' },
  ],
  'crates/agent/src/runtime/compaction.rs': [
    { function: 'compaction_projection', reason: 'read-only projection; compaction has no ledger write authority' },
    { function: 'budgeted_summarizer', reason: 'read-only budget prediction at compaction admission' },
  ],
  'crates/agent/src/runtime/retrieval.rs': [
    { function: 'latest_attempt_record', reason: 'read-only learning evidence selection' },
    { function: 'learning_corpus_evidence', reason: 'read-only auxiliary corpus evidence; unreadable id is non-fatal' },
  ],
  'crates/agent/src/runtime/verification_attribution.rs': [
    { function: 'run_independent_review_call', reason: 'read-only routing attribution for the paid review; unreadable id refuses typed' },
  ],
  'crates/agent/src/outcome_ledger.rs': [],
  'crates/orchestrator/src/runtime_helpers.rs': [
    { function: 'child_task_budget_cap', reason: 'read-only registry budget projection; unreadable id yields None' },
  ],
});

export const BANNED_FILES = Object.freeze(Object.keys(SETTLEMENT_ALLOWLIST));

function read(root, rel) {
  const path = join(root, rel);
  return existsSync(path) ? readFileSync(path, 'utf8') : '';
}

/**
 * Replace comments with spaces, keeping offsets and newlines. A doc comment
 * saying "re-reading task_id() would be wrong" is documentation, not a read.
 */
export function maskComments(text) {
  const out = [];
  let quote = null;
  for (let index = 0; index < text.length; index += 1) {
    const ch = text[index];
    const next = text[index + 1];
    if (quote) {
      out.push(ch);
      if (ch === '\\') {
        out.push(next ?? '');
        index += 1;
      } else if (ch === quote) {
        quote = null;
      }
      continue;
    }
    if (ch === '"') {
      quote = ch;
      out.push(ch);
      continue;
    }
    if (ch === "'") {
      // A Rust CHAR literal closes within a few characters; an apostrophe in
      // a lifetime (`<'a>`) does not start a string and must not swallow the
      // comments that follow it.
      const literal = /^'(?:\\.|[^'\\])'/.exec(text.slice(index, index + 8));
      if (literal !== null) {
        out.push(literal[0]);
        index += literal[0].length - 1;
        continue;
      }
      out.push(ch);
      continue;
    }
    if (ch === '/' && next === '/') {
      while (index < text.length && text[index] !== '\n') {
        out.push(' ');
        index += 1;
      }
      if (index < text.length) out.push('\n');
      continue;
    }
    if (ch === '/' && next === '*') {
      out.push('  ');
      index += 2;
      while (index < text.length && !(text[index] === '*' && text[index + 1] === '/')) {
        out.push(text[index] === '\n' ? '\n' : ' ');
        index += 1;
      }
      if (index < text.length) out.push('  ');
      index += 1;
      continue;
    }
    out.push(ch);
  }
  return out.join('');
}

function callArguments(text, openParen) {
  let depth = 0;
  let quote = null;
  for (let index = openParen; index < text.length; index += 1) {
    const ch = text[index];
    if (quote) {
      if (ch === '\\') index += 1;
      else if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
      continue;
    }
    if (ch === '(') depth += 1;
    else if (ch === ')') {
      depth -= 1;
      if (depth === 0) return text.slice(openParen + 1, index);
    }
  }
  return '';
}

/** One linear pass: every matched `{` -> `}` pair, skipping strings. */
function bracePairs(text) {
  const pairs = new Map();
  const stack = [];
  let quote = null;
  for (let index = 0; index < text.length; index += 1) {
    const ch = text[index];
    if (quote) {
      if (ch === '\\') index += 1;
      else if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"') {
      quote = ch;
      continue;
    }
    if (ch === "'") {
      const literal = /^'(?:\\.|[^'\\])'/.exec(text.slice(index, index + 8));
      if (literal !== null) index += literal[0].length - 1;
      continue;
    }
    if (ch === '{') stack.push(index);
    else if (ch === '}') {
      const open = stack.pop();
      if (open !== undefined) pairs.set(open, index);
    }
  }
  return pairs;
}

/** Function ranges (`fn name(...) { ... }`) by brace matching. */
export function functionRanges(text) {
  const ranges = [];
  const pairs = bracePairs(text);
  const re = /\bfn\s+([A-Za-z0-9_]+)\s*(?:<[^>]*>)?\s*\(/g;
  for (const match of text.matchAll(re)) {
    const open = text.indexOf('{', match.index);
    if (open === -1) continue;
    const close = pairs.get(open);
    if (close === undefined) continue;
    ranges.push({ name: match[1], start: match.index, end: close + 1 });
  }
  return ranges;
}

export function containingFunction(ranges, offset) {
  for (const range of ranges) {
    if (offset >= range.start && offset <= range.end) return range.name;
  }
  return null;
}

function lazyReads(text) {
  const out = [];
  for (const match of text.matchAll(/(?:\.|\b)(?:task_id|task_id_lazy)\s*\(\s*\)/g)) {
    out.push({ offset: match.index, text: match[0] });
  }
  return out;
}

/**
 * Pure scanner over the settlement module and the turn driver. Returns
 * {passed, observed, reason}.
 */
export function scanSources({ settlement, drive, relative = 'settlement.rs' }) {
  const problems = [];
  const allowed = new Set(
    (SETTLEMENT_ALLOWLIST[relative] ?? []).map((entry) => entry.function),
  );
  const settlementMasked = maskComments(settlement);
  const driveMasked = maskComments(drive);
  const ranges = functionRanges(settlementMasked);
  const seen = [];
  for (const site of lazyReads(settlementMasked)) {
    const fn = containingFunction(ranges, site.offset) ?? '<top-level>';
    seen.push(fn);
    if (!allowed.has(fn)) {
      problems.push(`${relative}: lazy task_id() read inside ${fn} (settlement tail must use the admitted TaskId)`);
    }
  }
  for (const earlier of SETTLEMENT_ALLOWLIST[relative] ?? []) {
    if (!ranges.some((range) => range.name === earlier.function)) {
      problems.push(`${relative}: allowlisted accessor ${earlier.function} no longer exists (ratchet must shrink, not rot)`);
    }
  }
  if (seen.length > (SETTLEMENT_ALLOWLIST[relative] ?? []).length) {
    problems.push(
      `${relative}: ${seen.length} lazy task_id() reads exceed the reviewed ceiling ${(SETTLEMENT_ALLOWLIST[relative] ?? []).length}`,
    );
  }

  // The settlement tail itself must not call the lazy settlement helpers.
  const tail = functionRanges(driveMasked).find((range) => range.name === 'finish_logical_turn');
  if (tail) {
    const body = driveMasked.slice(tail.start, tail.end);
    for (const banned of ['session_task', 'apply_gate_to_task_row', 'sync_task_row']) {
      const pattern = new RegExp(`(?<![A-Za-z0-9_])${banned}\\(`, 'g');
      for (const match of body.matchAll(pattern)) {
        const args = callArguments(body, match.index + match[0].length - 1);
        const bound =
          args.includes('settlement_task_id') ||
          args.includes('_task_id') ||
          /(^|[,()\s])task_id([,()\s]|$)/.test(args);
        if (bound) continue;
        if (args.includes('handle.task_id()') || args.includes('handle.task_id()?')) {
          // A fresh read in the tail is exactly the bug class.
        }
        problems.push(
          `turn/drive.rs: finish_logical_turn calls ${banned}(...) without the admitted TaskId (offset ${tail.start + match.index})`,
        );
      }
    }
    const signature = driveMasked.slice(tail.start, Math.min(tail.end, tail.start + 600));
    const bindsParameter = /\btask_id\s*:\s*TaskId\b/.test(signature);
    const bindsLocal = /settlement_task_id\s*=\s*handle\.task_id\(\)/.test(body);
    if (!bindsParameter && !bindsLocal) {
      problems.push('turn/drive.rs: finish_logical_turn does not take/bind the admitted TaskId');
    }
    // Await-point sweep: every suspension inside the settlement tail is an
    // interleaving window. The admitted identity must already be bound; the
    // tail may not contain a fresh `handle.task_id()` read after the first.
    const awaitPoints = [...body.matchAll(/\.await\b/g)].length;
    const freshReads = [...body.matchAll(/handle\.task_id\(\s*\)/g)];
    const admissionEnd = body.search(/handle\.task_id\(\s*\)/);
    for (const read of freshReads) {
      if (admissionEnd !== -1 && read.index !== admissionEnd) {
        problems.push(
          `turn/drive.rs: finish_logical_turn re-reads handle.task_id() after admission (interleaving window at ${read.index})`,
        );
      }
    }
    tail.await_points = awaitPoints;
    tail.fresh_reads = freshReads.length;
  } else {
    problems.push('turn/drive.rs: finish_logical_turn not found (scan cannot prove the tail)');
  }

  return {
    passed: problems.length === 0,
    observed: {
      await_points: tail?.await_points ?? null,
      settlement_reads: seen.length,
      allowed_functions: [...allowed].sort(),
      functions: [...new Set(seen)].sort(),
      scanned: [relative, 'crates/agent/src/runtime/turn/drive.rs'],
    },
    reason: problems.length === 0 ? undefined : problems.join('; '),
    problems,
  };
}

export function scan({ root = ROOT } = {}) {
  const settlement = read(root, 'crates/agent/src/runtime/settlement.rs');
  const drive = read(root, 'crates/agent/src/runtime/turn/drive.rs');
  if (settlement.length === 0 || drive.length === 0) {
    return { passed: false, reason: 'settlement module or turn driver is missing', observed: {} };
  }
  const tail = scanSources({ settlement, drive, relative: 'crates/agent/src/runtime/settlement.rs' });
  const problems = [...(tail.problems ?? [])];
  const files = { 'crates/agent/src/runtime/settlement.rs': { reads: tail.observed?.settlement_reads ?? 0 } };
  for (const file of BANNED_FILES) {
    if (file === 'crates/agent/src/runtime/settlement.rs') continue;
    const text = read(root, file);
    if (text.length === 0) {
      problems.push(`${file}: banned settlement file is missing (scan cannot prove it)`);
      continue;
    }
    const testModule = /^#\[cfg\(test\)\]\s*\n(?:#\[[^\]]+\]\s*\n)*mod\s+/m.exec(text);
    const production = testModule ? text.slice(0, testModule.index) : text;
    const masked = maskComments(production);
    const ranges = functionRanges(masked);
    const allowed = new Set((SETTLEMENT_ALLOWLIST[file] ?? []).map((entry) => entry.function));
    let reads = 0;
    let annotated = 0;
    for (const site of lazyReads(masked)) {
      reads += 1;
      const fn = containingFunction(ranges, site.offset) ?? '<top-level>';
      const lineStart = production.lastIndexOf('\n', site.offset) + 1;
      const previousStart = lineStart === 0 ? 0 : production.lastIndexOf('\n', lineStart - 1) + 1;
      const lineText = production.slice(lineStart, production.indexOf('\n', site.offset) === -1 ? production.length : production.indexOf('\n', site.offset));
      const previousText = production.slice(previousStart, lineStart);
      const annotation = /allowed-task-id-read:\s*\S.{8,}/.test(`${previousText}\n${lineText}`);
      if (annotation) {
        annotated += 1;
        continue;
      }
      if (!allowed.has(fn)) {
        const line = production.slice(0, site.offset).split('\n').length;
        problems.push(`${file}:${line}: lazy task_id() read inside ${fn} (banned settlement path; annotate only a reviewed non-settlement projection)`);
      }
    }
    for (const entry of SETTLEMENT_ALLOWLIST[file] ?? []) {
      if (!ranges.some((range) => range.name === entry.function)) {
        problems.push(`${file}: allowlisted accessor ${entry.function} no longer exists (ratchet must shrink, not rot)`);
      }
    }
    files[file] = { reads, annotated };
  }
  return {
    passed: problems.length === 0,
    observed: {
      await_points: tail.observed?.await_points ?? null,
      files,
      settlement_reads: Object.values(files).reduce((sum, entry) => sum + entry.reads, 0),
    },
    reason: problems.length === 0 ? undefined : problems.join('; '),
    problems,
  };
}

export function selftest() {
  let failures = 0;
  const check = (name, condition) => {
    if (condition) console.log(`settlement selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`settlement selftest FAIL: ${name}`);
    }
  };
  const drive = `
    async fn finish_logical_turn(&self, handle: &S) -> R {
      let settlement_task_id = handle.task_id()?;
      let row = self.sync_task_row(handle, settlement_task_id, &ledger, None)?;
      Ok(())
    }`;
  const clean = scanSources({
    relative: 'fixture.rs',
    settlement: 'fn session_task(&self, h: &S) -> R { Ok(()) }',
    drive,
  });
  check('bound tail passes', clean.passed);
  check(
    'a lifetime apostrophe does not hide a following line comment',
    !maskComments("fn h<'a>(x: &'a S) { // task_id() is documented\n}").includes('task_id'),
  );
  check(
    'a char literal brace does not unbalance function ranges',
    functionRanges("fn h() { let c = '{'; }").length === 1 &&
      functionRanges("fn h() { let c = '{'; }")[0].end === "fn h() { let c = '{'; }".length,
  );
  const lazyTail = scanSources({
    relative: 'fixture.rs',
    settlement: 'fn session_task(&self, h: &S) -> R { Ok(()) }',
    drive: drive.replace('settlement_task_id, &ledger', 'handle.task_id()?, &ledger'),
  });
  check('lazy tail read fails', !lazyTail.passed);
  const extraRead = scanSources({
    relative: 'fixture.rs',
    settlement: `
      fn session_task(&self, h: &S) -> R { h.task_id() }
      fn settle_tail(&self, h: &S) -> R { let id = h.task_id()?; Ok(id) }`,
    drive,
  });
  check('a new settlement-tail lazy read fails', !extraRead.passed);
  const noBinding = scanSources({
    relative: 'fixture.rs',
    settlement: 'fn session_task(&self, h: &S) -> R { Ok(()) }',
    drive: drive.replace('let settlement_task_id = handle.task_id()?;', ''),
  });
  check('missing admission binding fails', !noBinding.passed);
  const real = scan({ root: ROOT });
  check(
    `real tree scan reports ${real.observed.settlement_reads ?? 0} production lazy read(s)`,
    typeof real.observed.settlement_reads === 'number',
  );
  if (failures > 0) {
    console.error(`settlement selftest: ${failures} failure(s)`);
    return 1;
  }
  console.log('settlement selftest: PASS');
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  if (process.argv.includes('--selftest')) {
    process.exitCode = selftest();
  } else {
    const result = scan({ root: ROOT });
    console.log(`settlement scan: ${result.passed ? 'PASS' : 'FAIL'}`);
    if (!result.passed) {
      console.error(result.reason);
      process.exitCode = 1;
    }
  }
}
