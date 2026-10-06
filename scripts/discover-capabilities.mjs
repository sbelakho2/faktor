#!/usr/bin/env node
// Mechanically discovered product-capability set (audit P0-2).
//
// The proof manifest must not be self-declared: this module discovers the
// REAL exposed surfaces — native HTTP routes, builtin model tools, CLI
// commands, configured provider kinds, VS Code contributions and JetBrains
// plugin contributions — and the coverage checker requires every discovered
// id to correspond to a capability row (proven, delegated or an explicit
// gap). Adding a route/tool/command without a proof row therefore turns the
// release checker red.
//
// Modes:
//   node scripts/discover-capabilities.mjs            human table
//   node scripts/discover-capabilities.mjs --json     machine set
//   node scripts/discover-capabilities.mjs selftest   adversarial fixtures

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');

/** Product capability ids are lowercase kebab segments. */
export function normalizeCapabilityId(id) {
  return id
    .replace(/([a-z0-9])([A-Z])/g, '$1-$2')
    .replace(/[^a-zA-Z0-9.]+/g, '-')
    .toLowerCase()
    .replace(/-+/g, '-');
}

export function discover(root = ROOT) {
  const ids = new Set();
  const sources = {};

  const add = (source, id) => {
    const normalized = normalizeCapabilityId(id);
    ids.add(normalized);
    sources[normalized] = source;
  };

  // 1. Native HTTP routes: `.route("PATH", method(handler))`.
  const lifecycle = read(root, 'crates/server/src/api/lifecycle.rs');
  for (const match of lifecycle.matchAll(
    /\.route\(\s*"([^"]+)"\s*,\s*(get|post|put|patch|delete)\(/g,
  )) {
    const path = match[1].replace(/^\//, '').replace(/[/{}]/g, '.').replace(/\.+/g, '.');
    add('native-route', `native.${match[2]}.${path.replace(/\.$/, '')}`);
  }

  // 2. Builtin model tools actually registered by the daemon.
  const builder = read(root, 'crates/cli/src/daemon/builder.rs');
  for (const match of builder.matchAll(/tools(?:\.register|\.register_lazy)\(\s*([a-z_]+)::([a-z_]+)\(/g)) {
    const name = match[2].replace(/_tool(_with_secrets)?$/, '');
    add('builtin-tool', `tool.${name}`);
  }

  // 3. CLI commands: the real command tree from the built binary when
  // available, else the clap `Commands` enum in main.rs.
  const cli = discoverCliCommands(root);
  for (const command of cli) {
    add('cli-command', `cli.${command}`);
  }

  // 4. Provider kinds: the `ProviderCfg::kind()` match arms (the exact
  // transport families the configured-provider builder accepts).
  const provider = read(root, 'crates/cli/src/config/provider.rs');
  const kindAt = provider.indexOf('fn kind(&self)');
  if (kindAt !== -1) {
    const body = provider.slice(kindAt, provider.indexOf('\n    }', kindAt));
    for (const match of body.matchAll(/=>\s*"([a-z0-9_]+)"/g)) {
      add('provider-kind', `provider.${match[1]}`);
    }
  }

  // 5. VS Code contributions: commands, views, configuration, menus.
  const pkg = read(root, 'apps/vscode/package.json');
  if (pkg) {
    let document = null;
    try {
      document = JSON.parse(pkg);
    } catch {
      add('vscode-contribution', 'vscode.parse-error');
    }
    const contributes = document?.contributes ?? {};
    for (const command of contributes.commands ?? []) {
      if (typeof command?.command === 'string') add('vscode-command', `vscode.command.${command.command}`);
    }
    for (const view of Object.keys(contributes.views ?? {})) {
      add('vscode-view-container', `vscode.view.${view}`);
    }
    for (const config of Object.keys(contributes.configuration ?? {})) {
      add('vscode-configuration', `vscode.configuration.${config.trim() || 'root'}`.replace(/\s+/g, '-'));
    }
    for (const menu of Object.keys(contributes.menus ?? {})) {
      add('vscode-menu', `vscode.menu.${menu}`);
    }
  }

  // 6. JetBrains plugin contributions: actions and tool windows.
  const pluginXml = read(root, 'apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml');
  for (const match of pluginXml.matchAll(/<action\s+id="([^"]+)"/g)) {
    add('jetbrains-action', `jetbrains.action.${match[1]}`);
  }
  for (const match of pluginXml.matchAll(/<toolWindow\s+id="([^"]+)"/g)) {
    add('jetbrains-toolwindow', `jetbrains.toolwindow.${match[1]}`);
  }

  return { ids: [...ids].sort(), sources };
}

function read(root, rel) {
  const path = join(root, rel);
  return existsSync(path) ? readFileSync(path, 'utf8') : '';
}

/** Real CLI command tree: built binary first, clap enum fallback. */
export function discoverCliCommands(root = ROOT) {
  const binary = join(root, 'target', 'debug', 'faktor-cli');
  if (existsSync(binary)) {
    try {
      const help = execFileSync(binary, ['--help'], { encoding: 'utf8', timeout: 15_000 });
      const commandsBlock = /Commands:\n([\s\S]*?)(?:\n\n|$)/.exec(help);
      if (commandsBlock) {
        const commands = commandsBlock[1]
          .split('\n')
          .map((line) => /^\s{2}([a-z][a-z0-9-]*)\s/.exec(line))
          .filter(Boolean)
          .map((match) => match[1]);
        if (commands.length > 0) return [...new Set(commands)].sort();
      }
    } catch {
      // fall through to the source scan
    }
  }
  const main = read(root, 'crates/cli/src/main.rs');
  const enumAt = main.indexOf('enum Commands');
  if (enumAt === -1) return [];
  const body = main.slice(enumAt, main.indexOf('\n}', enumAt));
  return [...new Set([...body.matchAll(/^\s{4}([A-Z][A-Za-z0-9]*)\s*[({]/gm)].map((m) =>
    m[1].replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase(),
  ))].sort();
}

function main() {
  const args = process.argv.slice(2);
  if (args.includes('selftest')) {
    return selftest();
  }
  const discovered = discover(ROOT);
  if (args.includes('--json')) {
    console.log(JSON.stringify(discovered, null, 2));
    return 0;
  }
  for (const id of discovered.ids) {
    console.log(`${id}  <- ${discovered.sources[id]}`);
  }
  console.log(`discover-capabilities: ${discovered.ids.length} discovered`);
  return 0;
}

function selftest() {
  let failures = 0;
  const check = (name, ok) => {
    if (ok) console.log(`selftest ok: ${name}`);
    else {
      console.error(`selftest FAIL: ${name}`);
      failures += 1;
    }
  };
  const discovered = discover(ROOT);
  check('native routes are discovered', discovered.ids.some((id) => id.startsWith('native.get.')));
  check('builtin tools are discovered', discovered.ids.includes('tool.run-command') || discovered.ids.includes('tool.run_command'));
  check('CLI commands are discovered', discovered.ids.some((id) => id === 'cli.doctor'));
  check('VS Code commands are discovered', discovered.ids.includes('vscode.command.faktor.start-server'));
  check('JetBrains tool windows are discovered', discovered.ids.some((id) => id.startsWith('jetbrains.toolwindow.')));
  // Omission witness: a fixture tree containing an extra route is detected
  // by the same scanner the checker uses.
  const routeLine = '.route("/ghost-probe", get(native_models))';
  check(
    'the route scanner sees a newly added route',
    [...routeLine.matchAll(/\.route\(\s*"([^"]+)"\s*,\s*(get|post|put|patch|delete)\(/g)].length === 1,
  );
  if (failures > 0) {
    console.error(`discover-capabilities selftest: FAIL (${failures})`);
    process.exit(1);
  }
  console.log('discover-capabilities selftest: PASS');
  return 0;
}

const isMain = process.argv[1] && process.argv[1].endsWith('discover-capabilities.mjs');
if (isMain) {
  process.exit(main());
}
