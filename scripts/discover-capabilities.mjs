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

  // 1. Native HTTP routes: `.route("PATH", method(handler)…)*` in EVERY
  //    production server source. EVERY method of a chain is a real endpoint
  //    (audit P0-PROOF): the old first-method-only regex silently omitted
  //    the POST/PUT half of `get(...).post(...)` routes, and routes
  //    registered only in worker_plane.rs were invisible because only
  //    lifecycle.rs was read.
  for (const file of [
    'crates/server/src/api/lifecycle.rs',
    'crates/server/src/worker_plane.rs',
  ]) {
    const text = read(root, file);
    for (const match of text.matchAll(/\.route\s*\(/g)) {
      const open = match.index + match[0].length - 1;
      const body = balancedBody(text, open, '(', ')');
      if (body === null) continue;
      const pathMatch = /^\s*"([^"]+)"/.exec(body);
      if (!pathMatch) continue;
      const path = pathMatch[1].replace(/^\//, '').replace(/[/{}]/g, '.').replace(/\.+/g, '.');
      for (const method of body.matchAll(/\b(get|post|put|patch|delete)\s*\(/g)) {
        add('native-route', `native.${method[1]}.${path.replace(/\.$/, '')}`);
      }
    }
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
    // REAL view ids, not only the container (audit P0-PROOF): `views` maps a
    // container key to view descriptors whose `id` is the exposed surface.
    for (const [container, views] of Object.entries(contributes.views ?? {})) {
      add('vscode-view-container', `vscode.view.${container}`);
      for (const view of views ?? []) {
        if (typeof view?.id === 'string') add('vscode-view', `vscode.view.${view.id}`);
      }
    }
    // REAL settings, not the object bag keys (`title`/`properties`): every
    // `configuration` section's `title` and every `properties` key is an
    // exposed surface (the manifest may carry one section object or a list).
    const configuration = contributes.configuration ?? {};
    const sections = Array.isArray(configuration) ? configuration : [configuration];
    for (const section of sections) {
      if (typeof section?.title === 'string' && section.title.trim()) {
        add(
          'vscode-configuration',
          `vscode.configuration.${section.title.trim()}`.replace(/\s+/g, '-'),
        );
      }
      for (const key of Object.keys(section?.properties ?? {})) {
        add('vscode-configuration', `vscode.configuration.${key}`);
      }
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

/**
 * Body between the balanced pair opened at `openIndex`, skipping quoted
 * strings. Returns null when unbalanced. Used for `.route(...)` argument
 * lists and `enum { … }` blocks so scanning never stops at a nested call or
 * a `)` inside a handler expression.
 */
function balancedBody(text, openIndex, open, close) {
  if (text[openIndex] !== open) return null;
  let depth = 0;
  let quote = null;
  for (let i = openIndex; i < text.length; i += 1) {
    const ch = text[i];
    if (quote) {
      if (ch === '\\') i += 1;
      else if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"') {
      quote = ch;
      continue;
    }
    if (ch === "'") {
      // A Rust CHAR literal (`'{'`); an apostrophe in a doc comment is not a
      // quote and must not swallow the rest of the file.
      const literal = /^'(?:\\.|[^'\\])'/.exec(text.slice(i, i + 8));
      if (literal) i += literal[0].length - 1;
      continue;
    }
    if (ch === open) depth += 1;
    else if (ch === close) {
      depth -= 1;
      if (depth === 0) return text.slice(openIndex + 1, i);
    }
  }
  return null;
}

function kebab(name) {
  return name.replace(/([a-z0-9])([A-Z])/g, '$1-$2').toLowerCase();
}

/**
 * Parse `enum <Name> { … }` blocks into `Map<enumName, variantName[]>`.
 * Variants are the lines whose first token starts uppercase (fields start
 * lowercase); doc comments and attributes are skipped.
 */
function parseEnums(text) {
  const enums = new Map();
  for (const match of text.matchAll(/\benum\s+([A-Z][A-Za-z0-9]*)\s*\{/g)) {
    const open = match.index + match[0].length - 1;
    const body = balancedBody(text, open, '{', '}');
    if (body === null) continue;
    const variants = [];
    for (const line of body.split('\n')) {
      const variant = /^\s*([A-Z][A-Za-z0-9]*)\s*(?:[({,]|$)/u.exec(line);
      if (variant) {
        // Keep the variant line's remainder too: a tuple variant names its
        // subcommand enum ON the same line (`Artifact(EnterpriseArtifactAction)`).
        variants.push({
          name: variant[1],
          text: `${line.slice(line.indexOf(variant[1]) + variant[1].length)}\n`,
        });
      } else if (variants.length > 0) {
        variants[variants.length - 1].text += `${line}\n`;
      }
    }
    enums.set(match[1], variants);
  }
  return enums;
}

/**
 * The REAL clap command tree, parsed statically (audit P0-PROOF): the old
 * fallback searched for a non-existent `enum Commands` and the built-binary
 * path depended on an untracked target/debug artifact (the certifying lane
 * builds release and is node-only). Static enum parsing is hermetic, matches
 * clap's kebab rename (no `#[command(name=…)]` overrides exist) and also
 * discovers the SUBCOMMAND enums the help output never lists.
 */
export function discoverCliCommands(root = ROOT) {
  if (!read(root, 'crates/cli/src/main.rs')) return [];
  const enums = parseEnums(read(root, 'crates/cli/src/main.rs'));
  const top = enums.get('Command');
  if (!top) return [];
  const out = new Set();
  const visited = new Set();
  const expand = (prefix, variants, depth) => {
    if (depth > 4 || visited.has(prefix)) return;
    visited.add(prefix);
    for (const variant of variants) {
      const name = `${prefix}${kebab(variant.name)}`;
      out.add(name);
      // A payload naming another enum declared in the same file is a real
      // subcommand level (`commerce doctor`, `enterprise artifacts audit`).
      for (const reference of variant.text.matchAll(/[:(\s]([A-Z][A-Za-z0-9]*)/g)) {
        const sub = enums.get(reference[1]);
        if (!sub) continue;
        expand(`${name}.`, sub, depth + 1);
      }
    }
  };
  expand('', top, 0);
  return [...out].sort();
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
  // A chained route must expose EVERY method: the old first-method-only
  // scan saw only `get` here.
  const fixture = '.route("/ghost-probe", get(native_models).post(native_models))';
  const parsed = balancedBody(fixture, fixture.indexOf('('), '(', ')');
  const methods = [...parsed.matchAll(/\b(get|post|put|patch|delete)\s*\(/g)].map((m) => m[1]);
  check('the route scanner sees every method of a chained route', methods.join(',') === 'get,post');
  check('the CLI scanner parses the real command tree', discoverCliCommands(ROOT).includes('doctor'));
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
