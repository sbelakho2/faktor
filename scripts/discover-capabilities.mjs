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
// Discovery is CLOSED over the production module tree (audit P1 closure:
// "proof coverage may not define the universe it claims to prove"): native
// routes are collected from EVERY production `*.rs` file under
// `crates/server/src` — recursively, excluding any `tests/` path segment and
// any file name containing `test` — and builtin tools from EVERY production
// `*.rs` file under `crates/cli/src` with the same exclusions. A route or
// tool registered in a new/third module is therefore discovered even though
// no hand-written enumerating list names its file.
//
// Modes:
//   node scripts/discover-capabilities.mjs            human table
//   node scripts/discover-capabilities.mjs --json     machine set
//   node scripts/discover-capabilities.mjs selftest   adversarial fixtures

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
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
  //    production server source. Every method of a chain is a real endpoint
  //    (audit P0-PROOF): a first-method-only regex silently omitted the
  //    POST/PUT half of `get(...).post(...)` routes, and enumerating only
  //    lifecycle.rs/worker_plane.rs made a route in any other module
  //    invisible. Discovery walks the whole production module tree instead.
  for (const route of discoverNativeRoutes(root)) {
    add('native-route', route.id);
  }

  // 2. Builtin model tools actually registered by the daemon: every
  //    production CLI source, not one hand-picked builder file, so a tool
  //    registered from a new module is discovered too.
  for (const file of productionRustFiles(root, 'crates/cli/src')) {
    const text = read(root, file);
    for (const match of text.matchAll(/tools(?:\.register|\.register_lazy)\(\s*([a-z_]+)::([a-z_]+)\(/g)) {
      const name = match[2].replace(/_tool(_with_secrets)?$/, '');
      add('builtin-tool', `tool.${name}`);
    }
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
 * Every PRODUCTION `*.rs` file under `root/rel`, sorted and relative to
 * `root`: a recursive walk that skips any path segment equal to `tests` and
 * any file name containing `test`. Discovery over this list is closed over
 * the module tree: a new module is scanned without editing an enumerating
 * list.
 */
export function productionRustFiles(root, rel) {
  const out = [];
  const walk = (dir) => {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name === 'tests') continue;
        walk(path);
      } else if (
        entry.isFile() &&
        entry.name.endsWith('.rs') &&
        !entry.name.includes('test')
      ) {
        out.push(relative(root, path));
      }
    }
  };
  walk(join(root, rel));
  return out.sort();
}

/** Production server modules that may register native routes. */
export function serverRouteFiles(root = ROOT) {
  return productionRustFiles(root, 'crates/server/src');
}

/**
 * Native route ids from every `.route("PATH", method(handler)
 * [.method(handler)]*)` occurrence in `files`, with the source file kept for
 * diagnostics. EVERY method of a chain is a real endpoint; the default file
 * list is the WHOLE production server module tree.
 */
export function discoverNativeRoutes(root, files = serverRouteFiles(root)) {
  const found = new Map();
  for (const file of files) {
    const text = read(root, file);
    for (const match of text.matchAll(/\.route\s*\(/g)) {
      const open = match.index + match[0].length - 1;
      const body = balancedBody(text, open, '(', ')');
      if (body === null) continue;
      const pathMatch = /^\s*"([^"]+)"/.exec(body);
      if (!pathMatch) continue;
      const path = pathMatch[1].replace(/^\//, '').replace(/[/{}]/g, '.').replace(/\.+/g, '.');
      for (const method of body.matchAll(/\b(get|post|put|patch|delete)\s*\(/g)) {
        found.set(`native.${method[1]}.${path.replace(/\.$/, '')}`, file);
      }
    }
  }
  return [...found.keys()].sort().map((id) => ({ id, file: found.get(id) }));
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
  check('the CLI scanner parses the real command tree', discoverCliCommands(ROOT).includes('doctor'));

  // Omission witness (audit P1 closure): the scanner must see a chained route
  // planted in a synthetic THIRD module it was never told about, while still
  // refusing `tests/` segments and test-named files.
  const fixtureRoot = mkdtempSync(join(tmpdir(), 'faktor-discover-selftest-'));
  try {
    mkdirSync(join(fixtureRoot, 'crates/server/src/tests'), { recursive: true });
    writeFileSync(
      join(fixtureRoot, 'crates/server/src/third_module.rs'),
      '.route("/ghost-probe", get(ghost_handler).post(ghost_handler))\n',
    );
    writeFileSync(
      join(fixtureRoot, 'crates/server/src/ghost_routes_tests.rs'),
      '.route("/ghost-test", get(ghost_handler))\n',
    );
    writeFileSync(
      join(fixtureRoot, 'crates/server/src/tests/hidden.rs'),
      '.route("/ghost-hidden", get(ghost_handler))\n',
    );
    const synthetic = discoverNativeRoutes(fixtureRoot, serverRouteFiles(fixtureRoot)).map(
      (route) => route.id,
    );
    check(
      'a chained route in a synthetic third module yields every method',
      synthetic.includes('native.get.ghost-probe') && synthetic.includes('native.post.ghost-probe'),
    );
    check(
      'test-named files and tests/ segments stay excluded from discovery',
      !synthetic.some((id) => id.includes('ghost-test') || id.includes('ghost-hidden')),
    );
  } finally {
    rmSync(fixtureRoot, { recursive: true, force: true });
  }

  // The real tree may not yet have a route outside the historical two files;
  // in that case prove the walk is closed over the tree rather than over a
  // two-entry list by requiring the file-list helper to see more files.
  const routeFiles = serverRouteFiles(ROOT);
  const third = routeFiles.find(
    (file) =>
      !file.endsWith('api/lifecycle.rs') &&
      !file.endsWith('worker_plane.rs') &&
      /\.route\s*\(/.test(read(ROOT, file)),
  );
  if (third !== undefined) {
    const thirdIds = discoverNativeRoutes(ROOT, [third]).map((route) => route.id);
    check(`a route outside lifecycle.rs/worker_plane.rs is discovered (${third})`, thirdIds.length > 0);
  } else {
    check(
      `route discovery walks the whole production module tree (${routeFiles.length} files scanned)`,
      routeFiles.length > 2,
    );
  }

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
