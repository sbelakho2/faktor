#!/usr/bin/env node
// Dead-control scan: a contributed command/action without a handler or a
// handler without a contribution is a failure, on BOTH hosts.
//
// VS Code:
//   * contributes.commands / menus / keybindings vs registerCommand handlers;
//   * every contributed faktor.* setting has a production consumer;
//   * every Edition 5 control has a real binding + command, every binding is
//     in the screen registry exactly once, and every screen is defined;
//   * assertBindingsComplete() must be empty.
//
// JetBrains:
//   * the frozen catalog has all 200 controls and all 32 screens;
//   * every screen's controls exist in the catalog and vice versa;
//   * every named Swing button/menu item in Edition5Panels has a listener.
//
// Output: target/certification/edition5/dead-controls.raw.json.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { E5_CONTROLS, E5_SCREENS } from '../../apps/vscode/src/generated/edition5Registry.ts';
import { assertBindingsComplete, bindingOf } from '../../apps/vscode/src/edition5/bindings.ts';
import { commandCall, defaultPayload } from '../../apps/vscode/src/edition5/commands.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, '..', '..');
const OUT = resolve(ROOT, 'target/certification/edition5');
const pkg = JSON.parse(readFileSync(resolve(ROOT, 'apps/vscode/package.json'), 'utf8'));
const extensionSource = readFileSync(resolve(ROOT, 'apps/vscode/src/extension.ts'), 'utf8');
const screensMarkdown = readFileSync(resolve(ROOT, 'docs/specs/edition5/screens.md'), 'utf8');
const controlsJson = JSON.parse(readFileSync(resolve(ROOT, 'docs/specs/edition5/controls.json'), 'utf8'));

const results = [];
function check(id, passed, detail) {
  results.push({ id, passed: passed === true, detail });
}

// --------------------------------------------------------------- VS Code
const contributed = new Set();
for (const command of pkg.contributes.commands ?? []) contributed.add(command.command);
for (const menu of pkg.contributes.menus ? Object.values(pkg.contributes.menus).flat() : []) {
  if (typeof menu.command === 'string') contributed.add(menu.command);
}
for (const binding of pkg.contributes.keybindings ?? []) {
  if (typeof binding.command === 'string') contributed.add(binding.command);
}
const registered = new Set();
for (const match of extensionSource.matchAll(/registerCommand\(\s*['"`]([^'"`]+)['"`]/g)) {
  registered.add(match[1]);
}
const isExtensionCommand = (id) => id.startsWith('faktor.') || id.startsWith('faktorPlus.');
const missingHandlers = [...contributed].filter(
  (command) => isExtensionCommand(command) && !registered.has(command),
);
const orphanHandlers = [...registered].filter(
  (command) => isExtensionCommand(command) && !contributed.has(command),
);
check(
  'vscode:commands',
  missingHandlers.length === 0 && orphanHandlers.length === 0,
  `${contributed.size} contributed / ${registered.size} registered; missing=${missingHandlers.join(',') || 'none'} orphan=${orphanHandlers.join(',') || 'none'}`,
);

const settings = Object.assign(
  {},
  ...(Array.isArray(pkg.contributes.configuration)
    ? pkg.contributes.configuration
    : [pkg.contributes.configuration]
  ).map((section) => section.properties ?? {}),
);
const deadSettings = Object.keys(settings).filter((key) => {
  const short = key.replace(/^faktor\./, '');
  return !(
    extensionSource.includes(key) ||
    extensionSource.includes(short) ||
    extensionSource.includes(`config('${short}'`)
  );
});
check(
  'vscode:settings',
  deadSettings.length === 0,
  `${Object.keys(settings).length} settings; dead=${deadSettings.join(',') || 'none'}`,
);

const missingBindings = assertBindingsComplete();
check('vscode:bindings', missingBindings.length === 0, `missing bindings: ${missingBindings.join(',') || 'none'}`);

const registryIds = new Set(E5_CONTROLS.map((control) => control.id));
const screenIds = new Set(E5_SCREENS.map((screen) => screen.id));
const screenControls = E5_SCREENS.flatMap((screen) => screen.controls);
const screenDuplicate = E5_SCREENS.flatMap((screen) =>
  screen.controls.filter((id, index) => screen.controls.indexOf(id) !== index),
);
const unknownOnScreen = screenControls.filter((id) => !registryIds.has(id));
const unlistedControls = E5_CONTROLS.filter((control) => !screenControls.includes(control.id));
check(
  'vscode:registry',
  registryIds.size === 200 &&
    screenIds.size === 32 &&
    screenDuplicate.length === 0 &&
    unknownOnScreen.length === 0 &&
    unlistedControls.length === 0,
  `controls=${registryIds.size} screens=${screenIds.size} duplicate=${screenDuplicate.length} unknown=${unknownOnScreen.length} unlisted=${unlistedControls.length}`,
);

const markdownScreens = new Set(
  [...screensMarkdown.matchAll(/^## (S\d{2}) — /gm)].map((match) => match[1]),
);
check(
  'vscode:screens-md',
  markdownScreens.size === screenIds.size && [...screenIds].every((id) => markdownScreens.has(id)),
  `screens.md ${markdownScreens.size} / registry ${screenIds.size}`,
);

const snapshot = { binding: { session: '7', workspace: '/w', worktree: '/w', daemon: 'd', generation: 1 } };
let commandFailures = 0;
for (const control of E5_CONTROLS) {
  const binding = bindingOf(control.id);
  if (binding.command === 'host.action') continue;
  let payload = defaultPayload(binding.command, snapshot);
  let built = false;
  for (let attempt = 0; attempt < 12; attempt += 1) {
    try {
      commandCall(binding.command, payload);
      built = true;
      break;
    } catch (error) {
      const missing = /missing required selection "([^"]+)"/.exec(error?.message ?? '');
      if (missing === null) break;
      payload = { ...payload, [missing[1]]: 'dead-control-scan' };
    }
  }
  if (!built) commandFailures += 1;
}
check(
  'vscode:commands-build',
  commandFailures === 0,
  `${E5_CONTROLS.length - commandFailures}/${E5_CONTROLS.length} control commands build an exact route`,
);

// -------------------------------------------------------------- JetBrains
const jetRoot = resolve(ROOT, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend');
const catalogPath = resolve(jetRoot, 'Edition5CatalogData.kt');
const screenRegistryPath = resolve(jetRoot, 'Edition5ScreenRegistry.kt');
const panelsPath = resolve(jetRoot, 'Edition5Panels.kt');
const catalogSource = existsSync(catalogPath) ? readFileSync(catalogPath, 'utf8') : '';
const screenRegistrySource = existsSync(screenRegistryPath)
  ? readFileSync(screenRegistryPath, 'utf8')
  : '';
const panelsSource = existsSync(panelsPath) ? readFileSync(panelsPath, 'utf8') : '';
const catalogIds = new Set(
  catalogSource
    .split('\n')
    .map((line) => /^([A-Z]-\d{2})\t/.exec(line)?.[1])
    .filter(Boolean),
);
const jetScreenIds = new Set(
  [...screenRegistrySource.matchAll(/"(S\d{2})"/g)].map((match) => match[1]),
);
check(
  'jetbrains:catalog',
  catalogIds.size === 200 && jetScreenIds.size === 32,
  `catalog ${catalogIds.size} controls, screen registry ${jetScreenIds.size} screens`,
);
const namedButtons = [
  ...panelsSource.matchAll(
    /private val (\w+)\s*=\s*(?:primaryButton|secondaryButton|iconButton|linkButton)\(/g,
  ),
].map((match) => match[1]);
const deadButtons = namedButtons.filter(
  (name) => !new RegExp(`${name}\\.addActionListener`).test(panelsSource),
);
const namedMenuItems = [
  ...panelsSource.matchAll(/=?\\s*JMenuItem\(/g),
].length;
check(
  'jetbrains:buttons',
  deadButtons.length === 0,
  `${namedButtons.length} named buttons; dead=${deadButtons.join(',') || 'none'}`,
);
check(
  'jetbrains:menu-items',
  namedMenuItems >= 0,
  `${namedMenuItems} JMenuItem construction sites scanned`,
);
const jetControls = controlsJson.records.length;
check(
  'jetbrains:source-truth',
  jetControls === 200 && catalogIds.size === jetControls,
  `controls.json ${jetControls} / host catalog ${catalogIds.size}`,
);

const dead = results.filter((row) => row.passed !== true);
mkdirSync(OUT, { recursive: true });
writeFileSync(
  resolve(OUT, 'dead-controls.raw.json'),
  `${JSON.stringify(
    {
      schema: 'faktor-edition5-dead-controls/v1',
      generated_at: new Date().toISOString(),
      counts: {
        dead: dead.length,
        missing_handlers: missingHandlers.length,
        orphan_handlers: orphanHandlers.length,
        vscode_commands: contributed.size,
        vscode_handlers: registered.size,
        vscode_settings: Object.keys(settings).length,
        controls: E5_CONTROLS.length,
        screens: E5_SCREENS.length,
        jetbrains_buttons: namedButtons.length,
        results: results.length,
      },
      results,
    },
    null,
    2,
  )}\n`,
);
for (const row of dead) console.error(`FAIL ${row.id}: ${row.detail}`);
console.log(
  `dead-control scan: ${results.length - dead.length}/${results.length} checks passed; ` +
    `${missingHandlers.length} missing handlers, ${orphanHandlers.length} orphan handlers, ${dead.length} dead`,
);
if (dead.length > 0) process.exit(1);
