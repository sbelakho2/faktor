#!/usr/bin/env node
// Capability/wiring matrix: prove every exposed surface reaches a real
// consumer, and fail on a surface that is only a name.
//
// Checks (all derived from the tree, not from a hand-maintained list):
//   1. VS Code: contributed command -> registration -> handler body that
//      calls a service/effect symbol (navigation-only commands are named).
//   2. VS Code: contributed setting -> consumer (`config('<suffix>')`, the
//      full key, or an explicit manifest evidence string).
//   3. Webview: every posted message type has a host `case`.
//   4. JetBrains: plugin contributions -> class file; panel `service.x()`
//      calls -> a `fun x(` on the service class.
//   5. Native route -> Faktor-owned client literal/segment sequence or an
//      explicit `unwired` entry with a reason (never a silent route).
//   6. Config descriptor -> runtime-consumer table entry; every `applied`
//      key has a production reference outside the descriptor/table files.
//
// Unknown surfaces are PROBLEMS. Known, explicitly recorded `unwired`
// entries are WARNINGS and make `--release` fail. Stale entries are
// PROBLEMS. `selftest()` plants one violation per check and proves the
// scanner catches it, then proves the corrected fixture is clean.

import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import { discoverNativeRoutes } from '../../../scripts/discover-capabilities.mjs';
import { bindingOf8 } from '../../../apps/vscode/src/edition8/bindings.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
export const ROOT = resolve(HERE, '..', '..', '..');
export const MANIFEST = 'tests/programme/wiring-matrix.json';
export const SCHEMA = 'faktor-wiring-matrix/v1';

/** Commands whose effect IS navigation (opening a Faktor-owned surface). */
const NAVIGATION_COMMANDS = new Set([
  'faktor.openChat',
  'faktor.openControlCenter',
  'faktor.openModelPicker',
  'faktor.openSettings',
  'faktor.openEdition8',
  'faktor.openInspect8',
  'faktor.openHistory8',
  'faktor.openUsage8',
  'faktor.openSettings8',
  'faktor.openProviders8',
  'faktor.openTools8',
  'faktor.openContext8',
  'faktor.openDiagnostics8',
  'faktor.openControl8',
  'faktor.openAgentPicker8',
]);

/** A handler body must name one of these seams to count as wired. */
const EFFECT_SEAM =
  /(?:\b(?:client|backend|service|native|runtime\.deps|deps\.(?:backend|client|service))\s*\.|startDaemon\s*\(|stopDaemon\s*\(|resolveDaemon\s*\(|cancelActiveRun\s*\(|cancelTaskRun\s*\(|replyPermission|refreshFromUserGesture\s*\(|recoverEventStream\s*\(|controlPlaneSignIn\s*\(|controlPlaneSignOut\s*\(|locateDaemon\s*\(|newTaskFromCommand\s*\(|switchSessionFromCommand\s*\(|readCurrent\s*\(|applyPatch\s*\(|startTaskRun\s*\(|uploadAttachment\s*\(|captureBrowser\s*\(|trustExecutable\s*\(|deleteMemory\s*\(|testHook\s*\(|stopScoped\s*\(|switchConversationSafe\s*\(|grantCredits\s*\(|requestApproval\s*\(|resolveApproval\s*\(|addContextDraft\s*\()/;

function read(root, rel) {
  const path = join(root, rel);
  return existsSync(path) ? readFileSync(path, 'utf8') : null;
}

function walk(root, rel, { extensions = /\.(ts|kt|js|mjs|rs)$/, skipTestNamed = true } = {}) {
  const base = join(root, rel);
  const out = [];
  if (!existsSync(base)) return out;
  const visit = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (['target', 'node_modules', 'out', 'build', '.git'].includes(entry.name)) continue;
      const path = join(dir, entry.name);
      if (entry.isDirectory()) visit(path);
      else if (extensions.test(entry.name) && !(skipTestNamed && /test/i.test(entry.name))) {
        out.push(path);
      }
    }
  };
  visit(base);
  return out;
}

function balanced(text, openIndex, opener = '(', closer = ')') {
  let depth = 0;
  let quote = null;
  let lineComment = false;
  let blockComment = false;
  for (let index = openIndex; index < text.length; index += 1) {
    const char = text[index];
    const next = text[index + 1];
    if (lineComment) {
      if (char === '\n') lineComment = false;
      continue;
    }
    if (blockComment) {
      if (char === '*' && next === '/') {
        blockComment = false;
        index += 1;
      }
      continue;
    }
    if (quote) {
      if (char === '\\') index += 1;
      else if (char === quote) quote = null;
      continue;
    }
    if (char === '/' && next === '/') {
      lineComment = true;
      index += 1;
      continue;
    }
    if (char === '/' && next === '*') {
      blockComment = true;
      index += 1;
      continue;
    }
    if (char === '"' || char === "'" || char === '`') {
      quote = char;
      continue;
    }
    if (char === opener) depth += 1;
    else if (char === closer) {
      depth -= 1;
      if (depth === 0) return text.slice(openIndex + 1, index);
    }
  }
  return null;
}

function routePattern(path) {
  return path
    .split('/')
    .map((segment) =>
      segment.startsWith('{') || segment.startsWith(':')
        ? '[^/"\'`\\s]+'
        : segment.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'),
    )
    .join('/');
}

function routeSuffixSequence(path) {
  const variable = path.search(/\{[^}]+\}|:[A-Za-z_]+/);
  if (variable === -1) return null;
  const prefixEnd = path.lastIndexOf('/', variable);
  if (prefixEnd === -1) return null;
  const prefix = path.slice(0, prefixEnd + 1);
  const statics = path
    .slice(prefixEnd + 1)
    .split('/')
    .filter((segment) => segment && !segment.startsWith('{') && !segment.startsWith(':'));
  return statics.length === 0 ? null : { prefix, statics };
}

function clientText(root) {
  const files = [
    ...walk(root, 'apps/vscode/src'),
    ...walk(root, 'apps/jetbrains/backend/src'),
    ...walk(root, 'apps/jetbrains/frontend/src'),
    ...walk(root, 'crates/cli/src'),
  ];
  return files.map((file) => readFileSync(file, 'utf8')).join('\n');
}

function routeConsumed(route, text) {
  if (new RegExp(routePattern(route.path)).test(text)) return true;
  const suffix = routeSuffixSequence(route.path);
  if (!suffix) return false;
  if (!text.includes(suffix.prefix)) return false;
  let at = 0;
  for (const segment of suffix.statics) {
    const index = text.indexOf(`/${segment}`, at);
    if (index === -1) return false;
    at = index + 1;
  }
  return true;
}

function loadManifest(root) {
  const text = read(root, MANIFEST);
  if (text === null) return { schema: SCHEMA, routes: {}, configEvidence: {}, configUnwired: {}, controls: {} };
  return JSON.parse(text);
}

function commandRegistrations(root) {
  const registrations = new Map();
  const sources = [
    'apps/vscode/src/extension.ts',
    'apps/vscode/src/edition8/commands.ts',
    'apps/vscode/src/lspDiagnostics.ts',
  ];
  for (const rel of sources) {
    const text = read(root, rel);
    if (text === null) continue;
    const pattern =
      /(?:vscode\.commands\.)?register(?:Command)?\(\s*['"]([^'"]+)['"]\s*,\s*(?:async\s*)?(?:\([^)]*\)\s*=>|function\b|([A-Za-z_$][\w$]*))/g;
    for (const match of text.matchAll(pattern)) {
      const id = match[1];
      if (!registrations.has(id)) registrations.set(id, []);
      registrations.get(id).push({ file: rel, text, index: match.index });
    }
  }
  return registrations;
}

function functionBody(text, name) {
  const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const declaration = new RegExp(`(?:async\\s+)?function\\s+${escaped}\\s*\\(`).exec(text);
  if (declaration) {
    const params = balanced(text, text.indexOf('(', declaration.index));
    if (params !== null) {
      const paramsEnd = text.indexOf('(', declaration.index) + params.length + 2;
      const bodyStart = text.indexOf('{', paramsEnd);
      if (bodyStart !== -1) {
        const body = balanced(text, bodyStart, '{', '}');
        if (body !== null) return { index: declaration.index, body };
      }
    }
  }
  const assigned = new RegExp(
    `(?:const|let|var)\\s+${escaped}\\s*=\\s*(?:async\\s*)?\\([^)]*\\)(?:\\s*:\\s*[^=]+)?\\s*=>`,
  ).exec(text);
  if (assigned) {
    const arrowEnd = assigned.index + assigned[0].length;
    const blockStart = text.indexOf('{', arrowEnd);
    if (blockStart !== -1 && text.slice(arrowEnd, blockStart).trim() === '') {
      const body = balanced(text, blockStart, '{', '}');
      if (body !== null) return { index: assigned.index, body };
    }
    const semi = text.indexOf(';', arrowEnd);
    if (semi !== -1) return { index: assigned.index, body: text.slice(arrowEnd, semi) };
  }
  return null;
}

function handlerBody(text, registration) {
  const tail = text.slice(registration.index);
  const callOpen = tail.indexOf('(');
  const call = callOpen === -1 ? null : balanced(tail, callOpen);
  if (call === null) return '';
  const arrow = call.indexOf('=>');
  const inline = arrow === -1 ? '' : call.slice(arrow + 2);
  const blockStart = inline.indexOf('{');
  const block = blockStart === -1 ? inline : balanced(inline, blockStart, '{', '}') ?? inline;
  const resolved = [];
  for (const match of block.matchAll(/\b([A-Za-z_$][\w$]*)\s*\(/g)) {
    const found = functionBody(text, match[1]);
    if (found) resolved.push(found.body);
  }
  return [block, ...resolved].join('\n');
}

function vscodeProblems(root, manifest) {
  const pkgText = read(root, 'apps/vscode/package.json');
  if (pkgText === null) return { problems: [], warnings: [], counts: {} };
  const problems = [];
  const warnings = [];
  const pkg = JSON.parse(pkgText);
  const commands = pkg.contributes?.commands ?? [];
  const contributed = new Set(commands.map((entry) => entry.command));
  const registrations = commandRegistrations(root);
  const activation = new Set(pkg.activationEvents ?? []);
  for (const entry of commands) {
    const id = entry.command;
    const found = registrations.get(id) ?? [];
    if (found.length === 0) {
      problems.push(`vscode command ${id}: contributed but not registered`);
      continue;
    }
    if (found.length > 1) {
      problems.push(`vscode command ${id}: registered ${found.length} times`);
    }
    if (!activation.has(`onCommand:${id}`)) {
      problems.push(`vscode command ${id}: missing onCommand activation event`);
    }
    if (NAVIGATION_COMMANDS.has(id)) continue;
    const bodies = found.map((registration) => handlerBody(registration.text, registration));
    if (!bodies.some((body) => EFFECT_SEAM.test(body))) {
      if (process.env.WIRING_DEBUG) {
        console.error(`DEBUG ${id}: ${bodies.map((body) => body.slice(0, 500)).join('\n---\n')}`);
      }
      problems.push(`vscode command ${id}: handler only touches UI (no service/effect seam)`);
    }
  }
  for (const id of registrations.keys()) {
    if (!contributed.has(id)) {
      problems.push(`vscode command ${id}: registered but not contributed`);
    }
  }

  const configuration = pkg.contributes?.configuration ?? {};
  const sections = Array.isArray(configuration) ? configuration : [configuration];
  const src = [
    ...walk(root, 'apps/vscode/src'),
    ...walk(root, 'apps/vscode/media', { extensions: /\.(js|ts)$/, skipTestNamed: false }),
  ]
    .map((file) => readFileSync(file, 'utf8'))
    .join('\n');
  let settings = 0;
  for (const section of sections) {
    for (const key of Object.keys(section.properties ?? {})) {
      settings += 1;
      const suffix = key.replace(/^faktor\./, '');
      const evidence = manifest.settings?.[key];
      const quotedSuffix = new RegExp(`['"]${suffix.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}['"]`);
      const consumed =
        src.includes(key) ||
        quotedSuffix.test(src) ||
        (typeof evidence === 'string' && src.includes(evidence));
      if (!consumed) problems.push(`vscode setting ${key}: no consumer in the extension`);
    }
  }

  const chat = read(root, 'apps/vscode/media/chat.js');
  const extension = read(root, 'apps/vscode/src/extension.ts');
  if (chat !== null && extension !== null) {
    const sent = new Set([...chat.matchAll(/type\s*:\s*['"]([A-Za-z0-9_-]+)['"]/g)].map((m) => m[1]));
    const handled = new Set([...extension.matchAll(/case\s+['"]([A-Za-z0-9_-]+)['"]\s*:/g)].map((m) => m[1]));
    for (const type of sent) {
      if (!handled.has(type)) problems.push(`webview message ${type}: posted by chat.js but no host case`);
    }
  }
  return {
    problems,
    warnings,
    counts: { commands: commands.length, settings, messages: true },
  };
}

function jetbrainsProblems(root) {
  const plugin = read(root, 'apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml');
  if (plugin === null) return { problems: [], warnings: [], counts: {} };
  const problems = [];
  const classes = new Set();
  for (const file of walk(root, 'apps/jetbrains')) {
    const text = readFileSync(file, 'utf8');
    for (const match of text.matchAll(/\b(?:class|object)\s+([A-Za-z0-9_]+)/g)) classes.add(match[1]);
  }
  for (const match of plugin.matchAll(/(?:factoryClass|instance|class)="([^"]+)"/g)) {
    const simple = match[1].split('.').pop();
    if (!classes.has(simple)) {
      problems.push(`jetbrains contribution ${match[1]}: class source is missing`);
    }
  }
  const panel = read(root, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorChatPanel.kt');
  const service = read(root, 'apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/FaktorFrontendService.kt');
  if (panel !== null && service !== null) {
    const methods = new Set([...service.matchAll(/\bfun\s+([A-Za-z0-9_]+)\s*\(/g)].map((m) => m[1]));
    for (const match of panel.matchAll(/\bservice\.([A-Za-z0-9_]+)\s*\(/g)) {
      if (!methods.has(match[1])) {
        problems.push(`jetbrains panel service.${match[1]}(): no such method on FaktorFrontendService`);
      }
    }
  }
  return { problems, warnings: [], counts: { contributions: (plugin.match(/<(?:toolWindow|applicationConfigurable)\b/g) ?? []).length } };
}

function routeProblems(root, manifest) {
  const routes = discoverNativeRoutes(root);
  if (routes.length === 0) return { problems: [], warnings: [], counts: { routes: 0 } };
  const text = clientText(root);
  const problems = [];
  const warnings = [];
  const declared = manifest.routes ?? {};
  const seen = new Set();
  for (const route of routes) {
    const key = `${route.method.toUpperCase()} ${route.path}`;
    seen.add(key);
    if (routeConsumed(route, text)) continue;
    const entry = declared[key];
    if (!entry) {
      problems.push(`native route ${key}: no client consumes it and no wiring entry records why`);
      continue;
    }
    if (entry.state === 'unwired') {
      if (!entry.reason) problems.push(`native route ${key}: unwired entry has no reason`);
      else warnings.push(`native route ${key}: unwired (${entry.reason})`);
    } else if (entry.state !== 'wired') {
      problems.push(`native route ${key}: unknown wiring state ${JSON.stringify(entry.state)}`);
    }
  }
  for (const key of Object.keys(declared)) {
    if (!seen.has(key)) problems.push(`native route ${key}: wiring entry is stale (route no longer exists)`);
  }
  return {
    problems,
    warnings,
    counts: { routes: routes.length, declared: Object.keys(declared).length },
  };
}

function configProblems(root, manifest) {
  const full = read(root, 'crates/foundation/src/config.rs');
  const table = read(root, 'crates/foundation/src/config_consumers.rs');
  if (full === null || table === null) return { problems: [], warnings: [], counts: {} };
  const testAt = full.search(/\n#\[cfg\(test\)\]/);
  const production = testAt === -1 ? full : full.slice(0, testAt);
  const keys = [...new Set([...production.matchAll(/Descriptor::new\(\s*"([^"]+)"/g)].map((m) => m[1]))];
  const problems = [];
  const warnings = [];
  const applied = new Set();
  const stored = new Set();
  for (const key of keys) {
    const at = table.indexOf(`"${key}"`);
    if (at === -1) {
      problems.push(`config ${key}: no runtime-consumer classification`);
      continue;
    }
    const arrow = table.indexOf('=>', at);
    if (arrow === -1) {
      problems.push(`config ${key}: malformed runtime-consumer entry`);
      continue;
    }
    const after = table.slice(arrow, arrow + 240);
    const appliedAt = after.indexOf('applied(');
    const storedAt = after.indexOf('stored(');
    if (appliedAt !== -1 && (storedAt === -1 || appliedAt < storedAt)) applied.add(key);
    else if (storedAt !== -1) stored.add(key);
    else if (after.includes('RuntimeConsumer::Unknown')) {
      problems.push(`config ${key}: classified Unknown`);
    } else {
      problems.push(`config ${key}: unreadable runtime-consumer arm`);
    }
  }
  const files = [...walk(root, 'crates'), ...walk(root, 'apps')]
    .filter(
      (file) =>
        !file.endsWith('/foundation/src/config.rs') &&
        !file.endsWith('/foundation/src/config_consumers.rs'),
    )
    .map((file) => readFileSync(file, 'utf8'));
  for (const key of applied) {
    const evidence = manifest.configEvidence?.[key];
    const found = files.some((text) => text.includes(key));
    if (!found && !(typeof evidence === 'string' && evidence.length > 0)) {
      problems.push(`config ${key}: marked applied but no production consumer names it`);
    }
  }
  for (const key of Object.keys(manifest.configUnwired ?? {})) {
    if (!keys.includes(key)) problems.push(`config ${key}: unwired entry is stale`);
    else if (!stored.has(key)) problems.push(`config ${key}: recorded unwired but the table claims applied`);
  }
  for (const key of stored) warnings.push(`config ${key}: stored only (read-only; no runtime consumer)`);
  return { problems, warnings, counts: { configKeys: keys.length, applied: applied.size, storedOnly: stored.size } };
}

/** Scan one tree. `root` may be a synthetic fixture for the selftest. */

function controlsProblems(root, manifest) {
  const declared = manifest.controls ?? {};
  const ids = Object.keys(declared);
  if (ids.length === 0) return { problems: [], warnings: [], counts: { controls: 0 } };
  const problems = [];
  for (const [controlId, entry] of Object.entries(declared)) {
    let binding;
    try {
      binding = bindingOf8(controlId);
    } catch {
      problems.push(`edition8 control ${controlId}: wiring entry is stale (control no longer binds)`);
      continue;
    }
    const observed = {
      kind: binding.kind,
      method: binding.route.method,
      path: binding.route.path,
    };
    if (entry.kind !== observed.kind || entry.method !== observed.method || entry.path !== observed.path) {
      problems.push(
        `edition8 control ${controlId}: wiring entry ${entry.kind} ${entry.method} ${entry.path} ` +
          `does not match binding ${observed.kind} ${observed.method} ${observed.path}`,
      );
    }
    if (typeof entry.effect !== 'string' || entry.effect.length === 0) {
      problems.push(`edition8 control ${controlId}: wiring entry has no effect statement`);
    }
  }
  return { problems, warnings: [], counts: { controls: ids.length } };
}

export function scan(root = ROOT) {
  const manifest = loadManifest(root);
  const parts = [vscodeProblems(root, manifest), jetbrainsProblems(root), routeProblems(root, manifest), configProblems(root, manifest), controlsProblems(root, manifest)];
  const problems = parts.flatMap((part) => part.problems);
  const warnings = parts.flatMap((part) => part.warnings);
  const counts = Object.assign({}, ...parts.map((part) => part.counts));
  return { problems, warnings, counts };
}

/** Planted violations: one per check, then the corrected fixture is clean. */
export function selftest() {
  let failures = 0;
  const check = (name, condition, detail = '') => {
    if (condition) console.log(`wiring selftest ok: ${name}`);
    else {
      failures += 1;
      console.error(`wiring selftest FAIL: ${name}${detail ? ` (${detail})` : ''}`);
    }
  };
  const root = mkdtempSync(join(tmpdir(), 'faktor-wiring-'));
  const write = (rel, text) => {
    mkdirSync(dirname(join(root, rel)), { recursive: true });
    writeFileSync(join(root, rel), text);
  };
  write(
    'apps/vscode/package.json',
    JSON.stringify({
      activationEvents: ['onCommand:faktor.planted'],
      contributes: {
        commands: [{ command: 'faktor.planted', title: 'Planted' }],
        configuration: { properties: { 'faktor.planted.setting': { type: 'string' } } },
      },
    }),
  );
  write(
    'apps/vscode/src/extension.ts',
    "import * as vscode from 'vscode';\nexport function activate(context) {\n  context.subscriptions.push(vscode.commands.registerCommand('faktor.planted', () => {\n    void vscode.window.showInformationMessage('planted');\n  }));\n}\n",
  );
  write('apps/vscode/media/chat.js', "vscode.postMessage({ type: 'plantedMessage' });\n");
  write(
    'apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml',
    '<idea-plugin><extensions><toolWindow id="F" factoryClass="dev.faktor.frontend.PlantedFactory"/></extensions></idea-plugin>\n',
  );
  write('crates/server/src/api.rs', 'fn routes(r: Router) -> Router { r.route("/native/planted", get(handler)) }\n');
  write(
    'crates/foundation/src/config.rs',
    'pub fn default_descriptors() { let d = Descriptor::new("faktor.planted.key", "Planted", "x", "c", ValueKind::Boolean, Value::Bool(false), ApplyTiming::Effective); }\n',
  );
  write(
    'crates/foundation/src/config_consumers.rs',
    'pub fn runtime_consumer(key: &str) -> RuntimeConsumer { match key { "faktor.planted.key" => applied("planted"), _ => RuntimeConsumer::Unknown } }\n',
  );
  write('tests/programme/wiring-matrix.json', JSON.stringify({ schema: SCHEMA, routes: {}, configEvidence: {}, configUnwired: {} }));

  const before = scan(root);
  check('planted unregistered-looking command handler is caught', before.problems.some((p) => p.includes('faktor.planted') && p.includes('service/effect')), before.problems.join('; '));
  check('planted setting without a consumer is caught', before.problems.some((p) => p.includes('faktor.planted.setting')), before.problems.join('; '));
  check('planted webview message without a case is caught', before.problems.some((p) => p.includes('plantedMessage')), before.problems.join('; '));
  check('planted JetBrains class is caught', before.problems.some((p) => p.includes('PlantedFactory')), before.problems.join('; '));
  check('planted native route without a consumer is caught', before.problems.some((p) => p.includes('/native/planted')), before.problems.join('; '));
  check('planted config key without a consumer is caught', before.problems.some((p) => p.includes('faktor.planted.key')), before.problems.join('; '));

  write(
    'apps/vscode/src/extension.ts',
    "import * as vscode from 'vscode';\nexport function activate(context, client) {\n  context.subscriptions.push(vscode.commands.registerCommand('faktor.planted', async () => {\n    await client.startDaemon();\n  }));\n  const setting = vscode.workspace.getConfiguration('faktor').get('planted.setting');\n  void setting;\n}\n",
  );
  write('apps/vscode/media/chat.js', '');
  write(
    'apps/jetbrains/frontend/src/main/resources/META-INF/plugin.xml',
    '<idea-plugin><extensions><toolWindow id="F" factoryClass="dev.faktor.frontend.PlantedFactory"/></extensions></idea-plugin>\n',
  );
  write('apps/jetbrains/frontend/src/main/kotlin/dev/faktor/frontend/PlantedFactory.kt', 'package dev.faktor.frontend\nclass PlantedFactory\n');
  write('crates/server/src/api.rs', 'fn routes(r: Router) -> Router { r.route("/native/planted", get(handler)) }\n');
  write('apps/vscode/src/nativeClient.ts', "export const path = '/native/planted';\n");
  write(
    'crates/foundation/src/config.rs',
    'pub fn default_descriptors() { let d = Descriptor::new("faktor.planted.key", "Planted", "x", "c", ValueKind::Boolean, Value::Bool(false), ApplyTiming::Effective); }\n',
  );
  write(
    'crates/foundation/src/config_consumers.rs',
    'pub fn runtime_consumer(key: &str) -> RuntimeConsumer { match key { "faktor.planted.key" => applied("crates/planted.rs"), _ => RuntimeConsumer::Unknown } }\n',
  );
  write('crates/planted.rs', 'pub const KEY: &str = "faktor.planted.key";\n');
  write('apps/vscode/src/plantedConsumer.ts', "export const consumer = 'faktor.planted.key';\n");
  const after = scan(root);
  check('corrected fixture is clean', after.problems.length === 0, after.problems.join('; '));
  check('corrected fixture has no stale-entry problem', !after.problems.some((p) => p.includes('stale')));
  return failures === 0 ? 0 : 1;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  if (args.includes('--selftest')) process.exitCode = selftest();
  else {
    const result = scan(ROOT);
    for (const warning of result.warnings) console.error(`warning: ${warning}`);
    for (const problem of result.problems) console.error(`problem: ${problem}`);
    console.log(
      `wiring matrix: ${result.counts.commands ?? 0} commands, ${result.counts.settings ?? 0} settings, ${result.counts.controls ?? 0} edition8 controls, ${result.counts.routes ?? 0} routes, ${result.counts.configKeys ?? 0} config keys; ${result.problems.length} problems, ${result.warnings.length} warnings`,
    );
    process.exitCode = result.problems.length === 0 && !(args.includes('--release') && result.warnings.length > 0) ? 0 : 1;
  }
}
