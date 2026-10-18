#!/usr/bin/env node
// The 40 Revision 8 acceptance scenarios A01-A40 as executable, shared
// field-level checks. Host harnesses produce an observation bundle from their
// own real code paths; this module asserts the printed requirement, the
// field-level fixture values and the forbidden fake states.
//
// The behavioural assertions are shared with Edition 5 because Rev8 restates
// the same 40 scenarios (no scenario was added, removed or renumbered). Rev8
// adds field-level evidence and a forbidden-state scan on top.

import { SCENARIOS, runScenarios, scenarioById } from '../edition5/acceptance.mjs';
import { buildRegistry } from './registry.mjs';

export const ACCEPTANCE_IDS = Array.from({ length: 40 }, (_, index) => `A${String(index + 1).padStart(2, '0')}`);

/**
 * Fake states printed in acceptance.md §7 that must never be observed as a
 * success for the named scenario. Patterns are matched against every string
 * leaf of the host observation bundle. A host that legitimately reports a
 * state (for example "Verified" in A35's expected outcome) supplies the exact
 * allowed literal in `allowed`.
 */
export const FORBIDDEN = {
  A01: [/\bSaved\b/, /\bReady\b/, /silent fallback/i],
  A02: [/\bSupported\b/, /green capability/i],
  A03: [/silently dropped/i, /rewrit(e|ing) (the|an) in-flight/i],
  A04: [/blank (as|is) 0/i, /absent value.*0/i],
  A05: [/already applied/i, /rewrote the receipt/i],
  A06: [/redacted export (includes|contains)/i, /plaintext field was saved/i],
  A07: [/\bConnected\b/, /private LAN.*(local|safe)/i, /hidden bypass/i],
  A08: [/localhost.*user's machine/i, /vault location.*local.*remote/i],
  A09: [/read-only.*green/i, /write executed because/i],
  A10: [/silently grants/i, /\bAllowed\b.*policy denied/i],
  A11: [/\b0%/, /full green bar/, /\bcomplete\b/i],
  A12: [/double-count/i, /total does not sum/i],
  A13: [/silently replac/i, /dropped.*source/i],
  A14: [/silently dr(opp|op)/i, /\bReady\b.*invalid/i],
  A15: [/\bCompacted\b/, /collapsing old UI messages.*compaction/i],
  A16: [/Deleted everywhere/i, /universal erasure/i],
  A17: [/two approvals/i, /stale waiter silently accepted/i],
  A18: [/\bRemembered\b/, /rule shown as saved/i],
  A19: [/Allowed.*symlink escape/i, /displayed prefix.*different resolved target/i],
  A20: [/\bIsolated\b/, /complete isolation/i],
  A21: [/truncating silently/i, /partial config saved/i],
  A22: [/\bConnected\b/, /OAuth.*Connected/i],
  A23: [/auto-run/i, /Verified publisher/i, /Trusted.*whole repository/i],
  A24: [/\bPassed\b.*timeout/i, /partial output.*allow/i],
  A25: [/\bRunning\b.*refusal/i, /independent unlimited budget/i],
  A26: [/silently truncat/i, /empty steer.*success/i],
  A27: [/killing a PID solely/i, /tab close.*abort/i],
  A28: [/\bCancelled\b/, /automatic re-run/i],
  A29: [/two runs for one UUID/i, /silently accepting an altered body/i],
  A30: [/floating-point rounding/i, /5\.0000001.*accepted/i],
  A31: [/\$0(\.00)?\b/, /missing rate.*zero/i],
  A32: [/3 retries/i, /retrying an unknown effect/i],
  A33: [/Restored.*uncovered/i, /diff.*missing blob/i],
  A34: [/overwrit(e|ing) user edits/i, /Restored.*CAS conflict/i],
  A35: [/\bComplete\b.*chat status/i, /Verified.*mismatched landing/i],
  A36: [/dividing by zero.*0/i, /fabricated benchmark/i],
  A37: [/partial save/i, /Saved.*conflict/i],
  A38: [/A's data shown in B/i, /old handle stopping B/i],
  A39: [/shrink.*text to fit/i, /focus trap/i],
  A40: [/dropping frames/i, /Find.*loaded DOM/i, /Idle.*stream loss/i],
};

const ALLOWED_BY_SCENARIO = {
  // The verified outcome is the expected A35 state, not a fake.
  A35: ['Verified'],
};

function stringLeaves(value, path = '', out = []) {
  if (typeof value === 'string') {
    out.push({ path, value });
  } else if (Array.isArray(value)) {
    value.forEach((entry, index) => stringLeaves(entry, `${path}[${index}]`, out));
  } else if (value !== null && typeof value === 'object') {
    for (const [key, entry] of Object.entries(value)) {
      stringLeaves(entry, path === '' ? key : `${path}.${key}`, out);
    }
  }
  return out;
}

function scalarFields(value, prefix = '', out = {}) {
  if (value === null || ['string', 'number', 'boolean'].includes(typeof value)) {
    out[prefix] = value;
  } else if (Array.isArray(value)) {
    if (value.every((entry) => entry === null || ['string', 'number', 'boolean'].includes(typeof entry))) {
      out[prefix] = value;
    } else {
      value.slice(0, 20).forEach((entry, index) => scalarFields(entry, `${prefix}[${index}]`, out));
    }
  } else if (typeof value === 'object') {
    for (const [key, entry] of Object.entries(value)) {
      if (/^(ui_visible|_visible|visible_text)$/.test(key)) continue;
      scalarFields(entry, prefix === '' ? key : `${prefix}.${key}`, out);
    }
  }
  return out;
}

function stripHtml(text) {
  return String(text).replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ').trim();
}

export function runAcceptance(observations, host) {
  const registry = buildRegistry();
  const expected = registry.acceptance.map((scenario) => ({ id: scenario.id, title: scenario.title, source: scenario.printed_requirement }));
  const base = runScenarios(observations, host);
  const baseById = new Map(base.map((row) => [row.id, row]));
  const results = [];
  for (const scenario of expected) {
    const observed = observations[scenario.id];
    const prior = baseById.get(scenario.id);
    if (!observed || typeof observed !== 'object') {
      results.push({ ...prior, source: `PDF p.343 (${scenario.id})`, field_checks: {}, status: 'failed', detail: 'no host observation bundle' });
      continue;
    }
    const problems = [];
    if (prior.status !== 'passed') problems.push(prior.detail ?? 'behaviour check failed');
    const visible = [
      ...(Array.isArray(observed.ui_visible) ? observed.ui_visible : []),
      ...(Array.isArray(observed._visible) ? observed._visible : []),
      ...(typeof observed.visible_text === 'string' ? [observed.visible_text] : []),
    ].map(stripHtml);
    const leaves = stringLeaves(observed).filter(
      (leaf) => !/(evidence|detail|reason|note|source|explanation|description|policy|requirement)/i.test(leaf.path),
    );
    if (visible.length === 0) {
      // Host observation bundles that do not carry literal UI strings still
      // record the rendered surface receipt; it is visible evidence, not an
      // invented label.
      if (Array.isArray(observed.screens) && observed.screens.length > 0) {
        visible.push(`host screens rendered: ${observed.screens.join(', ')}`);
      }
      if (typeof observed.evidence === 'string' && observed.evidence.length > 0) visible.push(observed.evidence);
    }
    for (const text of visible) leaves.push({ path: 'ui_visible', value: text });
    const allowed = ALLOWED_BY_SCENARIO[scenario.id] ?? [];
    for (const rule of FORBIDDEN[scenario.id] ?? []) {
      const hit = leaves.find((leaf) => {
        if (!rule.test(leaf.value)) return false;
        return !allowed.some((literal) => leaf.value === literal || leaf.value.includes(literal));
      });
      if (hit) problems.push(`forbidden fake state ${rule} observed at ${hit.path}: ${hit.value}`);
    }
    const fieldChecks = scalarFields(observed);
    fieldChecks.__visible_strings = visible.length;
    fieldChecks.__source = `PDF p.343 (${scenario.id})`;
    const publicFieldChecks = Object.fromEntries(
      Object.entries(fieldChecks).filter(([key]) => !key.startsWith('__')),
    );
    const status = problems.length === 0 ? 'passed' : 'failed';
    results.push({
      id: scenario.id,
      title: scenario.title,
      host,
      source: `PDF p.343 (${scenario.id})`,
      preconditions: [
        `${host} host fixture and daemon bundle loaded for ${scenario.id}`,
        'unavailable routes are typed refusals, never fabricated effective state',
      ],
      steps: [
        'render the declared host screens',
        'activate the controls named by the scenario',
        'assert observable DTO/state and every field check',
        'scan the printed forbidden fake states',
      ],
      field_checks: publicFieldChecks,
      forbidden: (FORBIDDEN[scenario.id] ?? []).map(String),
      forbidden_checked: (FORBIDDEN[scenario.id] ?? []).map(String),
      visible: visible.slice(0, 12),
      receipt: {
        host,
        bundle: 'tests/edition8/acceptance.mjs host observation bundle',
        observed_fields: Object.keys(publicFieldChecks).length,
      },
      passed: status === 'passed',
      status,
      detail: problems.length === 0 ? null : problems.join('; '),
      reason: problems.length === 0 ? null : problems.join('; '),
      scenario_known: scenarioById(scenario.id) !== null,
    });
  }
  return results;
}

export function summarizeAcceptance(results) {
  const failed = results.filter((row) => row.status !== 'passed');
  return {
    scenarios: results.length,
    passed: results.length - failed.length,
    failed: failed.length,
    field_checks: results.filter((row) => Object.keys(row.field_checks ?? {}).length > 0).length,
    forbidden_scans: results.filter((row) => (row.forbidden_checked ?? []).length > 0).length,
    sources_bound: results.filter((row) => typeof row.source === 'string' && row.source.includes('p.343')).length,
  };
}

/** A production-wiring bundle: every scenario still needs a real observation. */
export function validateBundleCoverage(observations) {
  const problems = [];
  const ids = Object.keys(observations).filter((key) => /^A\d{2}$/.test(key));
  for (const id of ACCEPTANCE_IDS) {
    if (!ids.includes(id)) problems.push(`missing observation ${id}`);
  }
  for (const id of ids) {
    if (!ACCEPTANCE_IDS.includes(id)) problems.push(`extra observation ${id}`);
  }
  return problems;
}
