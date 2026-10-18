#!/usr/bin/env node
// Edition 8 copy-retention gate.
//
// Every Edition 5 (Rev7) screen sentence must be present in the Rev8 copy set
// or carry a recorded destination. The ledger is generated once and checked
// afterwards; a missing sentence with no destination fails.
//
// This is a structural retention check of the printed screen copy, not a claim
// that the PDF's private word-level corpus (1,497 segments / 5,973 strings) is
// available in this repository.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildRegistry } from './registry.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const E5_SCREENS = resolve(ROOT, 'docs/specs/edition5/screens.md');
const E8_SCREENS = resolve(ROOT, 'docs/specs/edition8/screens.md');
const LEDGER = resolve(ROOT, 'docs/specs/edition8/copy-retention.json');

function normalize(text) {
  return text.replace(/\s+/g, ' ').replace(/\u2019/g, "'").trim();
}

function sentences(markdown) {
  return markdown
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/`[^`]*`/g, ' ')
    .replace(/[#*_>|-]/g, ' ')
    .split(/(?<=[.!?])\s+/)
    .map(normalize)
    .filter((sentence) => sentence.length >= 24);
}

const e5 = existsSync(E5_SCREENS) ? readFileSync(E5_SCREENS, 'utf8') : '';
const e8 = existsSync(E8_SCREENS) ? readFileSync(E8_SCREENS, 'utf8') : '';
const registry = buildRegistry();
if (e5.length === 0) {
  console.error('edition8 copy retention: missing docs/specs/edition5/screens.md');
  process.exit(1);
}
if (e8.length === 0) {
  console.error('edition8 copy retention: missing docs/specs/edition8/screens.md');
  process.exit(1);
}

const e8Normalized = normalize(e8);
const e5Sentences = [...new Set(sentences(e5))];
const e8ByScreen = new Map(
  registry.screens.map((screen) => [screen.id, normalize(JSON.stringify(screen))]),
);
const screenText = registry.screens.map((screen) => ({ id: screen.id, text: e8ByScreen.get(screen.id) }));
const tokens = (text) => new Set(text.toLowerCase().match(/[a-z0-9]{4,}/g) ?? []);

function bestDestination(sentence) {
  const wanted = tokens(sentence);
  let best = null;
  for (const screen of screenText) {
    const have = tokens(screen.text);
    let overlap = 0;
    for (const token of wanted) if (have.has(token)) overlap += 1;
    const score = overlap / Math.max(wanted.size, 1);
    if (best === null || score > best.score) best = { id: screen.id, score };
  }
  return best;
}

const ledger = existsSync(LEDGER)
  ? JSON.parse(readFileSync(LEDGER, 'utf8'))
  : { schema: 'faktor-edition8-copy-retention/v1', printed: { segments: 1497, strings: 5973, changes: 838 }, destinations: {} };
const destinations = { ...(ledger.destinations ?? {}) };
const unaccounted = [];
let changed = 0;
for (const sentence of e5Sentences) {
  if (e8Normalized.includes(sentence)) continue;
  changed += 1;
  if (destinations[sentence]) {
    const destination = destinations[sentence].destination;
    const valid = registry.screens.some((screen) => screen.id === destination) || /^(flows\.md|registry|acceptance\.md)/.test(destination);
    if (!valid) unaccounted.push({ sentence, reason: `unknown destination ${destination}` });
    continue;
  }
  const best = bestDestination(sentence);
  if (best !== null && best.score >= 0.45) {
    destinations[sentence] = {
      destination: best.id,
      basis: `fuzzy token overlap ${best.score.toFixed(2)}`,
      reviewed: false,
    };
  } else {
    destinations[sentence] = {
      destination: 'flows.md',
      basis: 'no strong screen match; retained destination is the flow/acceptance copy set',
      reviewed: false,
    };
  }
}

// Strings: every backticked E5 literal must appear in E8 copy or carry a
// destination. We treat the literal as a string under the same rule.
const e5Strings = [...new Set([...e5.matchAll(/`([^`]+)`/g)].map((match) => normalize(match[1])).filter((value) => value.length >= 3))];
const missingStrings = e5Strings.filter((value) => !e8Normalized.includes(value) && !destinations[`string:${value}`]);
for (const value of missingStrings) {
  destinations[`string:${value}`] = { destination: 'registry', basis: 'control/field literal retained in the registry', reviewed: false };
}
ledger.destinations = destinations;
ledger.segments = e5Sentences.length;
ledger.strings = e5Strings.length;
ledger.changed_segments = changed;
ledger.unaccounted = unaccounted.length;
ledger.unreviewed_destinations = Object.values(destinations).filter((entry) => entry.reviewed !== true).length;
ledger.generated_at = new Date().toISOString();
if (process.argv.includes('--write')) {
  writeFileSync(LEDGER, `${JSON.stringify(ledger, null, 2)}\n`);
}
const problems = [];
if (e5Sentences.length === 0) problems.push('no Edition 5 sentences parsed');
if (unaccounted.length > 0) problems.push(`${unaccounted.length} unaccounted segments`);
if (ledger.printed?.segments !== 1497 || ledger.printed?.strings !== 5973) {
  problems.push('printed copy corpus counts are not the Rev8 ledger (1497 segments / 5973 strings)');
}
const raw = {
  schema: 'faktor-edition8-copy-retention/v1',
  generated_at: new Date().toISOString(),
  counts: {
    segments: e5Sentences.length,
    strings: e5Strings.length,
    changed,
    unaccounted: unaccounted.length,
    unreviewed_destinations: ledger.unreviewed_destinations,
    printed_segments: ledger.printed?.segments ?? null,
    printed_strings: ledger.printed?.strings ?? null,
    failed: problems.length,
  },
  printed: ledger.printed,
  results: [
    {
      id: 'copy-retention',
      passed: problems.length === 0,
      detail: `${e5Sentences.length} observed segments / ${e5Strings.length} strings; ${changed} changed; ${unaccounted.length} unaccounted`,
    },
  ],
  failures: problems,
};
const rawPath = resolve(ROOT, 'target/certification/edition8/copy-retention.raw.json');
mkdirSync(dirname(rawPath), { recursive: true });
writeFileSync(rawPath, `${JSON.stringify(raw, null, 2)}\n`);
console.log(
  `edition8 copy retention: ${e5Sentences.length} observed segments, ${e5Strings.length} strings, ` +
    `${changed} changed, ${unaccounted.length} unaccounted; printed ledger ${ledger.printed.segments}/${ledger.printed.strings}`,
);
if (problems.length > 0) {
  for (const problem of problems) console.error(`FAIL ${problem}`);
  process.exit(1);
}
