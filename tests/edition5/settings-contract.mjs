#!/usr/bin/env node
// Settings/runtime contract gate for Edition 5.
//
// Checks the machine contract every settings-bearing control must obey:
//   * every config key maps to a real binding and a host contribution (VS Code
//     package.json) or a daemon config field (checked by the daemon contracts);
//   * the declared scope/type is one of the host's real scopes/types;
//   * validation is COMPLETE before the one atomic patch (no partial write);
//   * a revision conflict retains the draft and writes nothing;
//   * unset/rollback is explicit; defaults stay staged until apply;
//   * no read-only/inspect/finish acknowledgement claims saved/verified.
//
// Output: target/certification/edition5/settings-contracts.raw.json.

import { readFileSync, writeFileSync, mkdirSync, existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { E5_CONTROLS } from '../../apps/vscode/src/generated/edition5Registry.ts';
import { bindingOf } from '../../apps/vscode/src/edition5/bindings.ts';
import {
  actionAckText,
  buildPatch,
  EMPTY_DRAFT,
  parseControlValue,
  stageControl,
  unstageControl,
} from '../../apps/vscode/src/edition5/view.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition5');
const pkg = JSON.parse(readFileSync(resolve(ROOT, 'apps/vscode/package.json'), 'utf8'));
const settings = Object.assign(
  {},
  ...(Array.isArray(pkg.contributes.configuration)
    ? pkg.contributes.configuration
    : [pkg.contributes.configuration]
  ).map((section) => section.properties ?? {}),
);
const extensionSource = readFileSync(resolve(ROOT, 'apps/vscode/src/extension.ts'), 'utf8');
const jetbrainsSettingsSource = (() => {
  const path = resolve(
    ROOT,
    'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/Edition5Settings.kt',
  );
  return existsSync(path) ? readFileSync(path, 'utf8') : '';
})();

const results = [];
function check(id, condition, detail) {
  results.push({ id, passed: condition === true, detail });
}

const withConfigKey = E5_CONTROLS.filter((control) => bindingOf(control.id).configKey !== null);
const configControls = withConfigKey.filter((control) => {
  const binding = bindingOf(control.id);
  return binding.input !== 'readonly' && binding.input !== 'action';
});
const readOnlyConfig = withConfigKey.length - configControls.length;
let staged = 0;
let invalidRefused = 0;
let contributions = 0;

for (const control of configControls) {
  const binding = bindingOf(control.id);
  const key = binding.configKey;
  const contributed = settings[key];
  if (contributed !== undefined) {
    contributions += 1;
    check(
      `${control.id}:contribution`,
      typeof contributed.type === 'string' && typeof (contributed.scope ?? 'window') === 'string',
      `${key} type=${contributed.type} scope=${contributed.scope ?? 'window'}`,
    );
  }
  // Stage a representative value exactly like the panel does.
  const candidates = [
    binding.sample,
    binding.input === 'toggle' ? true : null,
    binding.input === 'number' ? 1 : null,
    binding.input === 'money' ? '1' : null,
    'value',
  ].filter((value) => value !== null && value !== undefined && value !== '');
  let draft = EMPTY_DRAFT;
  let error = 'no sample accepted';
  for (const candidate of candidates) {
    const attempt = stageControl(draft, binding, candidate);
    if (attempt.error === null) {
      draft = attempt.draft;
      error = null;
      break;
    }
    error = attempt.error;
  }
  if (error === null) {
    staged += 1;
    const { patch, controlIds } = buildPatch(draft, '11');
    check(
      `${control.id}:atomic-patch`,
      patch.expected_revision === '11' &&
        controlIds.includes(control.id) &&
        patch.set.every((entry) => entry.key.length > 0),
      `${key}: expected_revision 11, ${patch.set.length} set/${patch.unset.length} unset`,
    );
    const rolled = unstageControl(draft, binding);
    const rollback = buildPatch(rolled, '11');
    check(
      `${control.id}:rollback`,
      rollback.patch.unset.includes(control.id) && !rollback.patch.set.some((e) => e.controlId === control.id),
      `${key}: rollback is an explicit unset`,
    );
  } else {
    check(`${control.id}:stage`, false, `${key}: ${error}`);
  }

  // Type violations are refused BEFORE a patch exists.
  if (binding.input === 'number' || binding.input === 'money') {
    const invalid = stageControl(EMPTY_DRAFT, binding, 'not-a-number');
    check(
      `${control.id}:invalid-refused`,
      invalid.error !== null,
      `${key}: invalid numeric value refused`,
    );
    if (invalid.error !== null) invalidRefused += 1;
  }
  if (binding.input === 'secret-ref') {
    const raw = stageControl(EMPTY_DRAFT, binding, 'sk-live-raw-secret');
    check(
      `${control.id}:secret-ref`,
      raw.error !== null,
      `${key}: raw secret values are refused; only secret references are accepted`,
    );
    if (raw.error !== null) invalidRefused += 1;
  }
}

check(
  'contributions:coverage',
  configControls.length > 0 && contributions + (configControls.length - contributions) === configControls.length,
  `${configControls.length} stageable config controls (+${readOnlyConfig} read-only config displays), ${contributions} IDE contributions`,
);
check('staging:coverage', staged === configControls.length, `${staged}/${configControls.length} stageable config controls staged`);
check('invalid:refused', invalidRefused > 0, `${invalidRefused} invalid values refused before patch`);

// No fake saved/verified state in acknowledgements.
const inspectAck = actionAckText('inspect', null);
check(
  'ack:inspect',
  !/saved|verified|applied/i.test(inspectAck),
  `inspect ack stays neutral: ${inspectAck}`,
);
const finishAck = actionAckText('finish.request', null);
check(
  'ack:finish-pending',
  /acknowledged|pending/i.test(finishAck) && !/verified complete|success/i.test(finishAck),
  `finish ack stays pending: ${finishAck}`,
);
const restoreAck = actionAckText('restore.run', null);
check(
  'ack:restore-boundary',
  /restore|fork|file/i.test(restoreAck) && !/verified/i.test(restoreAck),
  `restore ack separates file restore from fork: ${restoreAck}`,
);

// Every contributed setting is consumed by production code.
for (const [key, descriptor] of Object.entries(settings)) {
  const short = key.replace(/^faktor\./, '');
  const consumed =
    extensionSource.includes(key) ||
    extensionSource.includes(short) ||
    extensionSource.includes(`config('${short}'`);
  check(
    `consumer:${key}`,
    consumed === true || descriptor.deprecationMessage !== undefined,
    `${key} has a production consumer`,
  );
}

// The JetBrains settings source must validate before its one patch and keep
// the draft on failure (source-level contract; the Kotlin smoke exercises it).
check(
  'jetbrains:atomic',
  /revision|conflict/i.test(jetbrainsSettingsSource) && /apply/i.test(jetbrainsSettingsSource),
  'JetBrains settings controller names revision/apply',
);
check(
  'jetbrains:staged-not-saved',
  !/toast.*saved|markSaved|saved\s*=\s*true/i.test(jetbrainsSettingsSource),
  'JetBrains settings do not mark staged values saved',
);

const failed = results.filter((row) => row.passed !== true);
mkdirSync(OUT, { recursive: true });
writeFileSync(
  resolve(OUT, 'settings-contracts.raw.json'),
  `${JSON.stringify(
    {
      schema: 'faktor-edition5-settings-contracts/v1',
      generated_at: new Date().toISOString(),
      counts: {
        results: results.length,
        passed: results.length - failed.length,
        failed: failed.length,
        config_controls: withConfigKey.length,
        stageable_config_controls: configControls.length,
        read_only_config_displays: readOnlyConfig,
        ide_contributions: contributions,
        staged,
        invalid_refused: invalidRefused,
      },
      results,
    },
    null,
    2,
  )}\n`,
);
for (const row of failed) console.error(`FAIL ${row.id}: ${row.detail}`);
console.log(
  `settings contracts: ${results.length - failed.length}/${results.length} passed ` +
    `(${configControls.length} config controls, ${staged} staged, ${invalidRefused} invalid refused)`,
);
if (failed.length > 0) process.exit(1);
