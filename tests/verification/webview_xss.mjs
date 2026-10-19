#!/usr/bin/env node
// Static webview XSS/CSP boundary probe. Untrusted daemon text must only
// reach the DOM through text nodes; an inline <script> initial payload must
// be escaped for the script context; CSP must pin a nonce and forbid eval.
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const SOURCES = [join(ROOT, 'apps/vscode/src'), join(ROOT, 'apps/vscode/media'), join(ROOT, 'apps/jetbrains/frontend/src')];
const FORBIDDEN = [
  [/\.innerHTML\s*=/, 'innerHTML assignment'],
  [/\.outerHTML\s*=/, 'outerHTML assignment'],
  [/insertAdjacentHTML\s*\(/, 'insertAdjacentHTML'],
  [/document\.write\s*\(/, 'document.write'],
  [/\beval\s*\(/, 'eval'],
  [/new\s+Function\s*\(/, 'new Function'],
  [/dangerouslySetInnerHTML/, 'React raw HTML'],
];
function walk(dir, out = []) {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const entry of entries) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) walk(path, out);
    else if (/\.(?:ts|tsx|js|mjs|html)$/.test(entry.name) && statSync(path).isFile()) out.push(path);
  }
  return out;
}
const files = SOURCES.flatMap((source) => walk(source));
const failures = [];
const evidence = { files: files.length, sinks: [], csp: null, script_json: null };
for (const file of files) {
  const text = readFileSync(file, 'utf8');
  for (const [pattern, name] of FORBIDDEN) {
    if (pattern.test(text)) evidence.sinks.push(`${relative(ROOT, file)}: ${name}`);
  }
}
if (evidence.sinks.length > 0) failures.push(...evidence.sinks.map((sink) => `unsafe sink ${sink}`));
const htmlPath = join(ROOT, 'apps/vscode/src/edition8/html.ts');
const html = readFileSync(htmlPath, 'utf8');
const csp = /content="([^"]*Content-Security-Policy[^"]*)"/i.exec(html) ?? /Content-Security-Policy" content="([^"]+)"/i.exec(html);
const cspText = csp?.[1] ?? '';
evidence.csp = cspText;
if (!/script-src 'nonce-\$\{options\.nonce\}'/.test(cspText)) failures.push('CSP does not pin the script nonce');
if (/script-src[^;]*unsafe-(?:inline|eval)/.test(cspText)) failures.push('CSP weakens script-src');
if (!/default-src 'none'/.test(cspText)) failures.push('CSP is not default-deny');
const scriptJson = /function scriptJson\([\s\S]*?\n\}/.exec(html)?.[0] ?? '';
evidence.script_json = scriptJson.length > 0;
if (!scriptJson) failures.push('scriptJson helper missing');
for (const required of ['\\\\u003c', '\\\\u003e', '\\\\u0026', '\\\\u2028', '\\\\u2029']) {
  if (!scriptJson.includes(required.replaceAll('\\\\', '\\'))) failures.push(`scriptJson does not escape ${required}`);
}
if (!/window\.__faktorE8Initial = \$\{scriptJson\(initial\)\}/.test(html)) {
  failures.push('inline initial payload is not serialized through scriptJson');
}
const document = {
  schema: 'faktor-independent-webview-xss/v1',
  generated_at: new Date().toISOString(),
  files_scanned: evidence.files,
  unsafe_sinks: evidence.sinks,
  csp: evidence.csp,
  script_json: evidence.script_json,
  passed: failures.length === 0,
  failures,
};
console.log(JSON.stringify(document, null, 2));
if (failures.length > 0) process.exitCode = 1;
