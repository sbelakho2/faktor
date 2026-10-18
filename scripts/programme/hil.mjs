#!/usr/bin/env node
// Hardware-in-the-loop release lane driver.
//
// The report gates release on a real board rig (Renode or HIL). Without the
// rig's environment this is a typed refusal: the lane stays red rather than
// claiming a hardware result that never ran.

import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const OUT = resolve(ROOT, 'target/certification/programme/hil.json');

function main() {
  if (process.env.FAKTOR_HIL !== '1' || !process.env.FAKTOR_HIL_RIG) {
    console.error(
      'hil: FAKTOR_HIL=1 and FAKTOR_HIL_RIG=<rig command> are required; refusing to claim a hardware result',
    );
    return 2;
  }
  console.log(`$ ${process.env.FAKTOR_HIL_RIG}`);
  execFileSync('bash', ['-lc', process.env.FAKTOR_HIL_RIG], { cwd: ROOT, stdio: 'inherit' });
  return 0;
}

const code = main();
mkdirSync(dirname(OUT), { recursive: true });
writeFileSync(
  OUT,
  `${JSON.stringify(
    {
      schema: 'faktor-programme-hil/v1',
      requested: process.env.FAKTOR_HIL === '1',
      status: code === 0 ? 'passed' : 'refused',
      at: new Date().toISOString(),
    },
    null,
    2,
  )}\n`,
);
process.exitCode = code;
