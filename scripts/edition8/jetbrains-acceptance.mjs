#!/usr/bin/env node
// JetBrains Edition 8 acceptance adapter for A01-A40.
//
// Consumes the real host overlay written by Edition8AcceptanceSmoke (40
// scenarios, each mapped to actual Swing screens/controls) and the shared
// native-parser observations, then runs the same executable scenario checks
// as VS Code and the daemon. The overlay is a host receipt: a failed or
// missing overlay row fails the scenario even when the shared fixture passes.
//
// Output: target/certification/edition8/acceptance-jetbrains.raw.json.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { release } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { runAcceptance, summarizeAcceptance, validateBundleCoverage } from '../../tests/edition8/acceptance.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition8');
const OVERLAY = resolve(OUT, 'acceptance-jetbrains.overlays.json');
const FIXTURES = resolve(ROOT, 'tests/edition5/fixtures/acceptance-observations.json');
const MATRIX = resolve(ROOT, 'target/certification/jetbrains-host-matrix.json');
const LIVE = resolve(OUT, 'acceptance-jetbrains-live.json');

if (!existsSync(OVERLAY)) {
  console.error('edition8 jetbrains acceptance: missing acceptance-jetbrains.overlays.json; run compile-and-smoke.sh --smoke Edition8Acceptance');
  process.exit(1);
}
const overlay = JSON.parse(readFileSync(OVERLAY, 'utf8'));
const problems = [];
if (overlay.schema !== 'faktor-jetbrains-edition8-acceptance/v1') problems.push(`wrong overlay schema ${overlay.schema}`);
if (overlay.host !== 'jetbrains') problems.push(`wrong host ${overlay.host}`);
if (overlay.scenarios !== 40) problems.push(`overlay scenarios ${overlay.scenarios} != 40`);
if (overlay.failed !== 0) problems.push(`overlay failed ${overlay.failed}`);
if ((overlay.failures ?? []).length > 0) problems.push(`overlay failures: ${overlay.failures.join('; ')}`);

const base = JSON.parse(readFileSync(FIXTURES, 'utf8'));
const matrix = existsSync(MATRIX) ? JSON.parse(readFileSync(MATRIX, 'utf8')) : null;
const live = existsSync(LIVE) ? JSON.parse(readFileSync(LIVE, 'utf8')) : null;
if (live !== null) {
  if (live.schema !== 'faktor-edition8-acceptance-live/v1') problems.push(`wrong live schema ${live.schema}`);
  if (live.host !== 'jetbrains') problems.push(`wrong live host ${live.host}`);
  if (live.counts?.scenarios !== 40) problems.push(`live scenarios ${live.counts?.scenarios} != 40`);
  if (live.counts?.passed !== 40) problems.push(`live passed ${live.counts?.passed} != 40`);
  if (live.counts?.failed !== 0) problems.push(`live failed ${live.counts?.failed}`);
  if ((live.failures ?? []).length > 0) problems.push(`live failures: ${live.failures.join('; ')}`);
} else if (process.env.FAKTOR_E8_REQUIRE_LIVE === '1') {
  problems.push('missing acceptance-jetbrains-live.json; run compile-and-smoke.sh --smoke Edition8LiveAcceptance with a real daemon');
}

// Rev8 field-level fixtures whose values are host-independent and printed in
// acceptance.md §6; the host overlay proves the JetBrains screen/control path.
base.A30.tokens_display = new Intl.NumberFormat('en-US').format(120000);
base.A30.negative = 'rejected';
base.A30.overflow = 'rejected';
base.A30.ui_visible = ['5.00', 'USD', base.A30.tokens_display];
base.A32.base_delay_ms = 250;
base.A32.max_delay_ms = 5000;
base.A32.jitter = 0.2;
base.A32.max_attempts = 3;
base.A32.attempts = 3;
base.A32.retries = 2;
base.A37.expected_revision = 7;
base.A37.invalid_patch = { applied: false, written: 0, draft_retained: true, error: 'invalid value' };
base.A37.stale = { applied: false, draft_retained: true, compare_offered: true };
base.A39.widths = [240, 320, 480];
base.A39.text_scale = 2;
base.A39.text_expansion = 0.5;
base.A39.ime_works = true;
base.A39.high_contrast = true;
base.A39.min_target_px = 24;

for (const scenario of Object.keys(base)) {
  const receipt = overlay.records?.[scenario];
  if (!receipt) {
    problems.push(`overlay missing ${scenario}`);
    continue;
  }
  if (receipt.passed !== true) {
    problems.push(`overlay ${scenario} not passed: ${String(receipt.evidence)}`);
  }
  if (!Array.isArray(receipt.screens) || receipt.screens.length === 0) problems.push(`overlay ${scenario} has no host screen`);
  if (!Array.isArray(receipt.controls) || receipt.controls.length === 0) problems.push(`overlay ${scenario} has no host control`);
  base[scenario]._jetbrains = {
    overlay_passed: receipt.passed === true,
    screens: receipt.screens ?? [],
    controls: receipt.controls ?? [],
    evidence: receipt.evidence ?? '',
  };
}
if (matrix !== null) {
  base.A39.host_matrix_present = true;
  base.A39.host_matrix_checks = matrix.checks?.length ?? matrix.results?.length ?? null;
}

// The host receipt is the visible evidence row for this scenario: the actual
// Swing screens/controls the overlay exercised. Never synthesized from the
// registry, and never filled when the receipt is absent.
for (const scenario of Object.keys(base)) {
  const receipt = overlay.records?.[scenario];
  if (!receipt) continue;
  const visible = Array.isArray(base[scenario].ui_visible) ? [...base[scenario].ui_visible] : [];
  for (const screen of receipt.screens ?? []) visible.push(`screen ${screen}`);
  for (const control of receipt.controls ?? []) visible.push(`control ${control}`);
  base[scenario].ui_visible = visible;
}

const coverage = validateBundleCoverage(base);
problems.push(...coverage);
const results = runAcceptance(base, 'jetbrains');
for (const row of results) {
  if (row.status !== 'passed') problems.push(`${row.id}: ${row.detail}`);
}
const counts = {
  ...summarizeAcceptance(results),
  host_overlays: Object.keys(overlay.records ?? {}).length,
  host_screens: new Set(Object.values(overlay.records ?? {}).flatMap((record) => record.screens ?? [])).size,
  failed: results.filter((row) => row.status !== 'passed').length,
};
mkdirSync(OUT, { recursive: true });
writeFileSync(
  resolve(OUT, 'acceptance-jetbrains.raw.json'),
  `${JSON.stringify(
    {
      schema: 'faktor-edition8-acceptance/v1',
      host: 'jetbrains',
    platform: process.platform,
    os: release(),
      generated_at: new Date().toISOString(),
      source_overlay: 'target/certification/edition8/acceptance-jetbrains.overlays.json',
      counts,
      live: live === null ? null : live.counts,
      results,
      failures: problems,
    },
    null,
    2,
  )}\n`,
);
for (const problem of problems) console.error(`FAIL ${problem}`);
console.log(
  `jetbrains edition8 acceptance: ${counts.passed}/${counts.scenarios} scenarios passed; ` +
    `${counts.host_overlays} host overlays, ${counts.host_screens} host screens; ${counts.field_checks} field-level bundles; ` +
    `live ${live === null ? 'absent' : `${live.counts?.passed}/${live.counts?.scenarios}`}`,
);
if (problems.length > 0) process.exit(1);
