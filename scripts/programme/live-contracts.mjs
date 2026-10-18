#!/usr/bin/env node
// Weekly live lane driver.
//
// The offline half is always real (recorded provider fixtures). The live half
// is fail-closed: it runs only when the operator sets FAKTOR_LIVE_CONTRACTS=1
// and provides the account/key environment the canaries need. Missing
// configuration is a typed refusal, never a green "skipped" result.

import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const OUT = resolve(ROOT, 'target/certification/programme/live-contracts.json');

const LIVE_KEYS = ['OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'GITHUB_TOKEN', 'MOUSER_API_KEY', 'DIGIKEY_CLIENT_ID', 'LCSC_API_KEY'];

function run(command, argv) {
  console.log(`$ ${command} ${argv.join(' ')}`);
  execFileSync(command, argv, { cwd: ROOT, stdio: 'inherit' });
}

function main() {
  run('cargo', ['test', '-p', 'faktor-provider', '--all-features', '--lib']);
  if (process.env.FAKTOR_LIVE_CONTRACTS !== '1') {
    console.error(
      'live-contracts: FAKTOR_LIVE_CONTRACTS is not 1; the live half is a typed refusal (offline fixtures passed). No live result is claimed.',
    );
    return 2;
  }
  const missing = LIVE_KEYS.filter((key) => !process.env[key]);
  if (missing.length > 0) {
    console.error(`live-contracts: missing operator key(s): ${missing.join(', ')}; refusing to claim a live run`);
    return 2;
  }
  console.error(
    'live-contracts: operator keys are present but this checkout has no live canary driver wired; refusing to fabricate a live result',
  );
  return 2;
}

const code = main();
mkdirSync(dirname(OUT), { recursive: true });
writeFileSync(
  OUT,
  `${JSON.stringify(
    {
      schema: 'faktor-programme-live-contracts/v1',
      requested: process.env.FAKTOR_LIVE_CONTRACTS === '1',
      status: code === 0 ? 'passed' : 'refused',
      reason: code === 0 ? null : 'live operator configuration/driver absent or incomplete',
      at: new Date().toISOString(),
    },
    null,
    2,
  )}\n`,
);
process.exitCode = code;
