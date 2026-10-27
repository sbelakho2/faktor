#!/usr/bin/env node
// Static superlinearity budget for the shapes this audit fixed.
//
// The scan is deliberately narrower than "every nested loop": it pins the
// high-confidence, historically-regressed shapes (per-item transcript reads,
// per-blob governor lookups, per-append billing scans, per-child plan scans,
// and linear UI lookups inside loops). Legitimate bounded sites live in
// scripts/superlinearity-budget.json with a reason and measured bound; a new
// unlisted site fails the gate.
//
// Usage:
//   node scripts/check-superlinearity.mjs
//   node scripts/check-superlinearity.mjs --selftest
//   node scripts/check-superlinearity.mjs --list

import fs from 'node:fs';
import path from 'node:path';

const ROOT = process.cwd();
const BUDGET_PATH = path.join(ROOT, 'scripts', 'superlinearity-budget.json');
const RULES = [
  'rust-parts-per-item',
  'rust-governor-per-blob',
  'rust-period-full-bucket-scan',
  'rust-usage-full-row-scan',
  'rust-plan-position-per-child',
  'rust-position-in-loop',
  'ui-permission-linear-lookup',
  'ui-control-linear-lookup',
  'rust-assignment-per-item-find',
  'rust-abort-dedupe-scan',
  'rust-json-parse-per-fact',
  'ui-permission-inflight-linear',
  'ui-agent-catalog-find',
];

function isProduction(rel) {
  const parts = rel.split('/');
  if (parts.some((part) => part === 'target' || part === 'node_modules' || part === 'tests')) {
    return false;
  }
  const base = parts[parts.length - 1];
  if (/(^|_)tests?(_|\.|$)/.test(base)) return false;
  if (base === 'tests.rs' || base === 'test.rs') return false;
  return true;
}

function walk(dir, out) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    const rel = path.relative(ROOT, full).split(path.sep).join('/');
    if (!isProduction(rel)) continue;
    if (entry.isDirectory()) walk(full, out);
    else if (/\.(rs|ts|js|kt)$/.test(entry.name)) out.push(rel);
  }
}

function rustLines(text) {
  const lines = text.split('\n');
  let cutoff = lines.length;
  for (let index = 0; index < lines.length; index += 1) {
    if (/^\s*#\[cfg\(test\)\]/.test(lines[index])) {
      cutoff = index;
      break;
    }
  }
  return lines.slice(0, cutoff);
}

function functionBody(lines, signature) {
  const start = lines.findIndex((line) => line.includes(signature));
  if (start < 0) return '';
  let depth = 0;
  let seen = false;
  for (let index = start; index < lines.length; index += 1) {
    const line = lines[index];
    for (const char of line) {
      if (char === '{') {
        depth += 1;
        seen = true;
      } else if (char === '}') {
        depth -= 1;
      }
    }
    if (seen && depth === 0) return lines.slice(start, index + 1).join('\n');
  }
  return lines.slice(start, Math.min(lines.length, start + 200)).join('\n');
}

export function scanRustText(text, file = 'selftest.rs') {
  const lines = rustLines(text);
  const findings = [];
  const push = (rule, index, snippet) =>
    findings.push({ rule, file, line: index + 1, snippet: snippet.trim().slice(0, 160) });
  for (let index = 0; index < lines.length; index += 1) {
    if (!/\bfor\s+(?:\([^)]*\)|\w+)\s+in\b/.test(lines[index])) continue;
    const body = lines.slice(index, Math.min(lines.length, index + 40));
    for (let offset = 0; offset < body.length; offset += 1) {
      const line = body[offset];
      if (line.includes('.parts_of(')) {
        push('rust-parts-per-item', index + offset, line);
      }
      if (
        line.includes('.object_class_of_digest_in_family(') ||
        line.includes('.object_last_used(')
      ) {
        push('rust-governor-per-blob', index + offset, line);
      }
      if (line.includes('plan.work_items.iter().position')) {
        push('rust-plan-position-per-child', index + offset, line);
      } else if (line.includes('.iter().position(') || line.includes('.position(|')) {
        push('rust-position-in-loop', index + offset, line);
      }
      if (
        line.includes('rows.iter().find(|a| a.item_id == item.id)') ||
        line.includes('rows.iter().find(|row| row.item_id == item.id)')
      ) {
        push('rust-assignment-per-item-find', index + offset, line);
      }
      if (line.includes('op_ids.contains(&q)')) {
        push('rust-abort-dedupe-scan', index + offset, line);
      }
    }
  }
  const pointwise = functionBody(lines, 'fn apply_period_event_pointwise');
  if (pointwise.includes('read_period_buckets(')) {
    const at = lines.findIndex((line) => line.includes('fn apply_period_event_pointwise'));
    push('rust-period-full-bucket-scan', at, 'read_period_buckets() in pointwise append');
  }
  const usageAppend = functionBody(lines, 'fn append_usage_event');
  if (/state\.usage\s*\.values\(\)/.test(usageAppend)) {
    const at = lines.findIndex((line) => line.includes('fn append_usage_event'));
    push('rust-usage-full-row-scan', at, 'usage.values() scan in append');
  }
  const factIndex = functionBody(lines, 'fn new(facts:');
  if (factIndex.includes('let chunked = serde_json::from_str')) {
    const at = lines.findIndex((line) => line.includes('fn new(facts:'));
    push('rust-json-parse-per-fact', at, 'JSON decode per fact row in FactIndex::new');
  }
  const seen = new Set();
  return findings.filter((finding) => {
    const key = `${finding.rule}\u0000${finding.file}\u0000${finding.line}\u0000${finding.snippet}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

export function scanUiText(text, file) {
  const findings = [];
  if (text.includes('lastPermissionIds.indexOf(')) {
    findings.push({
      rule: 'ui-permission-linear-lookup',
      file,
      line: 1,
      snippet: 'lastPermissionIds.indexOf in renderPermissions',
    });
  }
  if (text.includes('live.indexOf(permissionRefusals')) {
    findings.push({
      rule: 'ui-permission-linear-lookup',
      file,
      line: 1,
      snippet: 'live.indexOf in prunePermissionRefusals',
    });
  }
  if (file.endsWith('edition8/host.ts') && text.includes('E8_CONTROLS.find(')) {
    findings.push({
      rule: 'ui-control-linear-lookup',
      file,
      line: 1,
      snippet: 'E8_CONTROLS.find per control lookup',
    });
  }
  if (
    text.includes('permissionInFlight.indexOf(') ||
    text.includes('permissionInFlight.push(')
  ) {
    findings.push({
      rule: 'ui-permission-inflight-linear',
      file,
      line: 1,
      snippet: 'permissionInFlight array scan per rendered card',
    });
  }
  if (text.includes('catalog.find((entry) => entry.provider === provider')) {
    findings.push({
      rule: 'ui-agent-catalog-find',
      file,
      line: 1,
      snippet: 'catalog.find per agent summary',
    });
  }
  return findings;
}

function scanRepository() {
  const files = [];
  walk(path.join(ROOT, 'crates'), files);
  walk(path.join(ROOT, 'apps', 'vscode', 'src'), files);
  walk(path.join(ROOT, 'apps', 'vscode', 'media'), files);
  walk(path.join(ROOT, 'apps', 'jetbrains', 'frontend', 'src', 'main', 'kotlin'), files);
  const findings = [];
  for (const rel of files) {
    const text = fs.readFileSync(path.join(ROOT, rel), 'utf8');
    if (rel.endsWith('.rs')) findings.push(...scanRustText(text, rel));
    else findings.push(...scanUiText(text, rel));
  }
  return findings;
}

function loadBudget() {
  return JSON.parse(fs.readFileSync(BUDGET_PATH, 'utf8'));
}

function evaluate(findings, budget) {
  const perRule = Object.fromEntries(RULES.map((rule) => [rule, []]));
  for (const finding of findings) {
    if (perRule[finding.rule] === undefined) continue;
    perRule[finding.rule].push(finding);
  }
  const allow = (budget.allowlist ?? []).map((entry) => ({ ...entry, remaining: entry.count ?? 1 }));
  const unallowed = [];
  for (const rule of RULES) {
    for (const finding of perRule[rule]) {
      const entry = allow.find(
        (candidate) =>
          candidate.rule === rule && candidate.file === finding.file && candidate.remaining > 0,
      );
      if (entry) entry.remaining -= 1;
      else unallowed.push(finding);
    }
  }
  const over = [];
  for (const rule of RULES) {
    const limit = budget.limits?.[rule] ?? 0;
    const count = unallowed.filter((finding) => finding.rule === rule).length;
    if (count > limit) over.push({ rule, limit, count });
  }
  return { perRule, unallowed, over };
}

function selftest() {
  const parts = scanRustText(
    'fn f() {\n  for row in rows {\n    let p = handle.parts_of(row.id)?;\n  }\n}\n',
  );
  if (!parts.some((finding) => finding.rule === 'rust-parts-per-item')) {
    throw new Error('selftest: per-item parts not detected');
  }
  const batched = scanRustText(
    'fn f() {\n  let ids = rows.iter().map(|r| r.id).collect::<Vec<_>>();\n  let mut p = handle.parts_for_messages(&ids)?;\n  for row in rows {\n    let x = p.remove(&row.id);\n  }\n}\n',
  );
  if (batched.some((finding) => finding.rule === 'rust-parts-per-item')) {
    throw new Error('selftest: batched parts falsely flagged');
  }
  const position = scanRustText(
    'fn f() {\n  for w in waiters {\n    let at = waiters.iter().position(|x| x == w);\n  }\n}\n',
  );
  if (!position.some((finding) => finding.rule === 'rust-position-in-loop')) {
    throw new Error('selftest: position-in-loop not detected');
  }
  const plan = scanRustText(
    'for row in children {\n  let idx = plan.work_items.iter().position(|w| w.id == row.item);\n}\n',
  );
  if (!plan.some((finding) => finding.rule === 'rust-plan-position-per-child')) {
    throw new Error('selftest: plan position not detected');
  }
  const pointwise = scanRustText(
    'fn apply_period_event_pointwise() {\n  let b = read_period_buckets();\n}\n',
  );
  if (!pointwise.some((finding) => finding.rule === 'rust-period-full-bucket-scan')) {
    throw new Error('selftest: period full scan not detected');
  }
  const usage = scanRustText(
    'fn append_usage_event() {\n  let x = state.usage.values().find(|r| r.id == id);\n}\n',
  );
  if (!usage.some((finding) => finding.rule === 'rust-usage-full-row-scan')) {
    throw new Error('selftest: usage scan not detected');
  }
  const assignment = scanRustText(
    'fn f() {\n  for item in items {\n    let row = rows.iter().find(|a| a.item_id == item.id);\n  }\n}\n',
  );
  if (!assignment.some((finding) => finding.rule === 'rust-assignment-per-item-find')) {
    throw new Error('selftest: assignment per-item find not detected');
  }
  const abort = scanRustText(
    'fn f() {\n  for q in queued {\n    if !op_ids.contains(&q) {}\n  }\n}\n',
  );
  if (!abort.some((finding) => finding.rule === 'rust-abort-dedupe-scan')) {
    throw new Error('selftest: abort dedupe scan not detected');
  }
  const fact = scanRustText(
    'fn new(facts: &[Row]) {\n  for (kind, key, value) in facts {\n    let chunked = serde_json::from_str::<Value>(value);\n  }\n}\n',
  );
  if (!fact.some((finding) => finding.rule === 'rust-json-parse-per-fact')) {
    throw new Error('selftest: per-fact JSON parse not detected');
  }
  const inflight = scanUiText('permissionInFlight.indexOf(id)', 'apps/vscode/media/chat.js');
  if (!inflight.some((finding) => finding.rule === 'ui-permission-inflight-linear')) {
    throw new Error('selftest: permission in-flight array scan not detected');
  }
  const catalog = scanUiText(
    'catalog.find((entry) => entry.provider === provider && entry.model === model)',
    'apps/vscode/src/state.ts',
  );
  if (!catalog.some((finding) => finding.rule === 'ui-agent-catalog-find')) {
    throw new Error('selftest: agent catalog scan not detected');
  }
  const ui = scanUiText('lastPermissionIds.indexOf(id)', 'apps/vscode/media/chat.js');
  if (!ui.some((finding) => finding.rule === 'ui-permission-linear-lookup')) {
    throw new Error('selftest: UI permission indexOf not detected');
  }
  const controls = scanUiText('E8_CONTROLS.find((x) => x.id === id)', 'apps/vscode/src/edition8/host.ts');
  if (!controls.some((finding) => finding.rule === 'ui-control-linear-lookup')) {
    throw new Error('selftest: UI control find not detected');
  }
  console.log('superlinearity selftest: OK');
}

function main() {
  const args = new Set(process.argv.slice(2));
  if (args.has('--selftest')) {
    selftest();
    return;
  }
  const budget = loadBudget();
  const findings = scanRepository();
  const { unallowed, over } = evaluate(findings, budget);
  if (args.has('--list')) {
    for (const finding of findings) {
      console.log(`${finding.file}:${finding.line} ${finding.rule} ${finding.snippet}`);
    }
  }
  if (over.length > 0) {
    console.error('superlinearity budget exceeded:');
    for (const item of over) {
      console.error(`  ${item.rule}: ${item.count} unlisted site(s), limit ${item.limit}`);
    }
    for (const finding of unallowed) {
      console.error(`  ${finding.file}:${finding.line} ${finding.rule} ${finding.snippet}`);
    }
    process.exit(1);
  }
  console.log(
    `superlinearity budget: OK (${findings.length} site(s), ${findings.length - unallowed.length} allowed, 0 over budget)`,
  );
}

main();
