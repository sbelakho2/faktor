#!/usr/bin/env node
// Canonical protocol codegen (audit 25).
//
// The ONE source of the plain protocol DTO/constant surface is
// `crates/protocol/src/schema.rs`, emitted to
// `crates/protocol/schema/faktor-protocol.schema.json`. This script
// regenerates the DTO/parser portion for TypeScript and Kotlin from that
// artifact. Handwritten behavior/UI code is never generated or touched.
//
// Modes:
//   --check          full gate: re-emit the artifact with cargo into a temp
//                    dir, diff it against the checked-in artifact, then
//                    regenerate both clients into temp dirs and diff them
//                    against the checked-in generated files. Exit 1 on drift.
//   --check-clients  node-only half: diff the clients against the checked-in
//                    artifact (used by node lanes that have no cargo).
//   --write          regenerate the artifact (cargo) and both clients in place.
//   --selftest       exercise the drift comparators offline (no cargo).
//
// The checked-in generated files are:
//   apps/vscode/src/generated/protocolDto.ts
//   apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/GeneratedProtocolDto.kt
//
// Generated vs handwritten is documented in
// `crates/protocol/schema/CODEGEN.md`.

import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const ARTIFACT = 'crates/protocol/schema/faktor-protocol.schema.json';
const TS_TARGET = 'apps/vscode/src/generated/protocolDto.ts';
const KT_TARGET =
  'apps/jetbrains/shared/src/main/kotlin/dev/faktor/shared/GeneratedProtocolDto.kt';

const BIN_ARGS = ['run', '-q', '-p', 'faktor-protocol', '--bin', 'faktor-protocol-schema', '--'];

function fail(message) {
  console.error(`protocol-codegen: ${message}`);
  process.exit(1);
}

function readSchema() {
  const text = readFileSync(resolve(ROOT, ARTIFACT), 'utf8');
  let schema;
  try {
    schema = JSON.parse(text);
  } catch (e) {
    fail(`${ARTIFACT} is not valid JSON: ${e.message}`);
  }
  if (schema.schema !== 'faktor-protocol-schema/v1') {
    fail(`${ARTIFACT} has unknown schema id ${JSON.stringify(schema.schema)}`);
  }
  return { text, schema };
}

function emitArtifactTo(path) {
  const run = spawnSync('cargo', [...BIN_ARGS, '--out', path], {
    cwd: ROOT,
    encoding: 'utf8',
  });
  if (run.error) {
    fail(
      `cannot run cargo to emit the protocol schema (${run.error.message}); ` +
        'use --check-clients on node-only hosts',
    );
  }
  if (run.status !== 0) {
    fail(`cargo schema emission failed (exit ${run.status}):\n${run.stderr || ''}`);
  }
}

// ---------------------------------------------------------------- utilities

function lineDiff(expected, actual) {
  const a = expected.split('\n');
  const b = actual.split('\n');
  const max = Math.max(a.length, b.length);
  for (let i = 0; i < max; i += 1) {
    if (a[i] !== b[i]) {
      return `first difference at line ${i + 1}:\n  expected: ${JSON.stringify(
        a[i] ?? '<eof>',
      )}\n  actual:   ${JSON.stringify(b[i] ?? '<eof>')}`;
    }
  }
  return 'no line difference (trailing bytes differ)';
}

function compare(path, expected, actual) {
  if (expected === actual) return false;
  console.error(`protocol-codegen: DRIFT at ${path}`);
  console.error(`  ${lineDiff(expected, actual)}`);
  return true;
}

function header(schema, commentOpen, commentClose) {
  return [
    `${commentOpen} GENERATED FILE - DO NOT EDIT BY HAND.`,
    `${commentOpen} Source: ${ARTIFACT} (schema ${schema.schema})`,
    `${commentOpen} Regenerate: node scripts/protocol-codegen.mjs --write`,
    `${commentOpen} The handwritten behavior/UI code that consumes these DTOs is`,
    `${commentOpen} NOT generated; see crates/protocol/schema/CODEGEN.md.`,
    commentClose,
  ].join('\n');
}

// ------------------------------------------------------------- TypeScript

const TS_HELPER_DECLS = [
  {
    name: 'ProtocolJson',
    deps: [],
    text: `export type ProtocolJson =
  | null
  | boolean
  | number
  | string
  | ProtocolJson[]
  | { [key: string]: ProtocolJson };`,
  },
  {
    name: 'ProtocolDtoError',
    deps: [],
    text: `export class ProtocolDtoError extends Error {
  readonly path: string;
  readonly detail: string;

  constructor(path: string, detail: string) {
    super(\`protocol DTO violation at \${path}: \${detail}\`);
    this.name = 'ProtocolDtoError';
    this.path = path;
    this.detail = detail;
  }
}`,
  },
  {
    name: 'ProtocolJsonObject',
    deps: ['ProtocolJson'],
    text: `type ProtocolJsonObject = { [key: string]: ProtocolJson };`,
  },
  {
    name: 'dtoFail',
    deps: ['ProtocolDtoError'],
    text: `function dtoFail(path: string, detail: string): never {
  throw new ProtocolDtoError(path, detail);
}`,
  },
  {
    name: 'dtoDescribe',
    deps: ['ProtocolJson'],
    text: `function dtoDescribe(value: ProtocolJson): string {
  if (value === null) return 'null';
  if (Array.isArray(value)) return 'an array';
  return typeof value;
}`,
  },
  {
    name: 'dtoObject',
    deps: ['ProtocolJson', 'ProtocolJsonObject', 'dtoFail', 'dtoDescribe'],
    text: `function dtoObject(value: ProtocolJson, path: string): ProtocolJsonObject {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    dtoFail(path, \`expected an object, got \${dtoDescribe(value)}\`);
  }
  return value as ProtocolJsonObject;
}`,
  },
  {
    name: 'dtoRequired',
    deps: ['ProtocolJson', 'ProtocolJsonObject', 'dtoFail'],
    text: `function dtoRequired(object: ProtocolJsonObject, key: string, path: string): ProtocolJson {
  if (!Object.prototype.hasOwnProperty.call(object, key)) {
    dtoFail(path, \`missing required field \${key}\`);
  }
  return object[key] as ProtocolJson;
}`,
  },
  {
    name: 'dtoOptional',
    deps: ['ProtocolJson', 'ProtocolJsonObject'],
    text: `function dtoOptional(object: ProtocolJsonObject, key: string): ProtocolJson | undefined {
  if (!Object.prototype.hasOwnProperty.call(object, key)) return undefined;
  return object[key] as ProtocolJson;
}`,
  },
  {
    name: 'dtoRejectUnknown',
    deps: ['ProtocolJsonObject', 'dtoFail'],
    text: `function dtoRejectUnknown(
  object: ProtocolJsonObject,
  path: string,
  allowed: readonly string[],
): void {
  for (const key of Object.keys(object)) {
    if (!allowed.includes(key)) {
      dtoFail(path, \`unknown field \${key}\`);
    }
  }
}`,
  },
  {
    name: 'dtoString',
    deps: ['ProtocolJsonObject', 'dtoRequired', 'dtoFail', 'dtoDescribe'],
    text: `function dtoString(object: ProtocolJsonObject, key: string, path: string): string {
  const value = dtoRequired(object, key, path);
  if (typeof value !== 'string') {
    dtoFail(\`\${path}.\${key}\`, \`expected a string, got \${dtoDescribe(value)}\`);
  }
  return value;
}`,
  },
  {
    name: 'dtoBool',
    deps: ['ProtocolJsonObject', 'dtoRequired', 'dtoFail', 'dtoDescribe'],
    text: `function dtoBool(object: ProtocolJsonObject, key: string, path: string): boolean {
  const value = dtoRequired(object, key, path);
  if (typeof value !== 'boolean') {
    dtoFail(\`\${path}.\${key}\`, \`expected a boolean, got \${dtoDescribe(value)}\`);
  }
  return value;
}`,
  },
  {
    name: 'dtoI64',
    deps: ['ProtocolJsonObject', 'dtoRequired', 'dtoFail', 'dtoDescribe'],
    text: `function dtoI64(object: ProtocolJsonObject, key: string, path: string): number {
  const value = dtoRequired(object, key, path);
  if (typeof value !== 'number' || !Number.isSafeInteger(value)) {
    dtoFail(\`\${path}.\${key}\`, \`expected a safe integer, got \${dtoDescribe(value)}\`);
  }
  return value;
}`,
  },
  {
    name: 'dtoI32',
    deps: ['ProtocolJsonObject', 'dtoI64', 'dtoFail'],
    text: `function dtoI32(object: ProtocolJsonObject, key: string, path: string): number {
  const value = dtoI64(object, key, path);
  if (value < -2147483648 || value > 2147483647) {
    dtoFail(\`\${path}.\${key}\`, \`integer \${value} exceeds i32 range\`);
  }
  return value;
}`,
  },
  {
    name: 'dtoList',
    deps: ['ProtocolJson', 'ProtocolJsonObject', 'dtoRequired', 'dtoFail', 'dtoDescribe'],
    text: `function dtoList(object: ProtocolJsonObject, key: string, path: string): ProtocolJson[] {
  const value = dtoRequired(object, key, path);
  if (!Array.isArray(value)) {
    dtoFail(\`\${path}.\${key}\`, \`expected an array, got \${dtoDescribe(value)}\`);
  }
  return value;
}`,
  },
  {
    name: 'dtoNullableString',
    deps: ['ProtocolJsonObject', 'dtoRequired', 'dtoFail', 'dtoDescribe'],
    text: `function dtoNullableString(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): string | null {
  const value = dtoRequired(object, key, path);
  if (value === null) return null;
  if (typeof value !== 'string') {
    dtoFail(\`\${path}.\${key}\`, \`expected a string or null, got \${dtoDescribe(value)}\`);
  }
  return value;
}`,
  },
  {
    name: 'dtoNullableI64',
    deps: ['ProtocolJsonObject', 'dtoRequired', 'dtoFail', 'dtoDescribe'],
    text: `function dtoNullableI64(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): number | null {
  const value = dtoRequired(object, key, path);
  if (value === null) return null;
  if (typeof value !== 'number' || !Number.isSafeInteger(value)) {
    dtoFail(\`\${path}.\${key}\`, \`expected a safe integer or null, got \${dtoDescribe(value)}\`);
  }
  return value;
}`,
  },
  {
    name: 'dtoNullableI32',
    deps: ['ProtocolJsonObject', 'dtoNullableI64', 'dtoFail'],
    text: `function dtoNullableI32(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): number | null {
  const value = dtoNullableI64(object, key, path);
  if (value !== null && (value < -2147483648 || value > 2147483647)) {
    dtoFail(\`\${path}.\${key}\`, \`integer \${value} exceeds i32 range\`);
  }
  return value;
}`,
  },
  {
    name: 'dtoOptionalNullableI64',
    deps: ['ProtocolJsonObject', 'dtoOptional', 'dtoFail', 'dtoDescribe'],
    text: `function dtoOptionalNullableI64(
  object: ProtocolJsonObject,
  key: string,
  path: string,
): number | null {
  const value = dtoOptional(object, key);
  if (value === undefined || value === null) return null;
  if (typeof value !== 'number' || !Number.isSafeInteger(value)) {
    dtoFail(\`\${path}.\${key}\`, \`expected a safe integer or null, got \${dtoDescribe(value)}\`);
  }
  return value;
}`,
  },
];

/** Only the helpers the generated body actually references (tsc runs with
 * noUnusedLocals); dependencies are pulled in transitively. */
function selectTsHelpers(body, schema) {
  const needed = new Set();
  const add = (name) => {
    if (needed.has(name)) return;
    const decl = TS_HELPER_DECLS.find((d) => d.name === name);
    if (!decl) fail(`unknown TS helper ${name}`);
    needed.add(name);
    for (const dep of decl.deps) add(dep);
  };
  for (const base of [
    'ProtocolJson',
    'ProtocolJsonObject',
    'dtoFail',
    'dtoDescribe',
    'dtoObject',
    'dtoRequired',
  ]) {
    add(base);
  }
  if (schema.types.some((t) => t.unknown_fields === 'reject')) add('dtoRejectUnknown');
  for (const decl of TS_HELPER_DECLS) {
    if (new RegExp(`\\b${decl.name}\\(`).test(body)) add(decl.name);
  }
  return TS_HELPER_DECLS.filter((d) => needed.has(d.name))
    .map((d) => d.text)
    .join('\n\n');
}

function pascal(name) {
  return name;
}

function tsType(kind) {
  switch (kind.kind) {
    case 'string':
      return 'string';
    case 'i64':
    case 'i32':
      return 'number';
    case 'bool':
      return 'boolean';
    case 'json':
      return 'ProtocolJson';
    case 'named':
      return `Protocol${pascal(kind.name)}`;
    case 'list':
      return `readonly ${tsType(kind.of)}[]`;
    default:
      fail(`unknown type kind ${kind.kind}`);
  }
}

function tsExpr(field, objectExpr, pathExpr) {
  const key = field.name;
  const base = `${objectExpr}, ${JSON.stringify(key)}, ${pathExpr}`;
  const child = `${pathExpr} + ".${key}"`;
  const nullable = field.nullable;
  const optional = field.optional;
  switch (field.type.kind) {
    case 'string':
      if (nullable) return `dtoNullableString(${base})`;
      return `dtoString(${base})`;
    case 'bool':
      return `dtoBool(${base})`;
    case 'i64':
      if (optional && nullable) return `dtoOptionalNullableI64(${base})`;
      if (nullable) return `dtoNullableI64(${base})`;
      return `dtoI64(${base})`;
    case 'i32':
      return nullable ? `dtoNullableI32(${base})` : `dtoI32(${base})`;
    case 'json':
      return `dtoRequired(${objectExpr}, ${JSON.stringify(key)}, ${pathExpr})`;
    case 'named': {
      const validate = `validateProtocol${pascal(field.type.name)}`;
      if (optional && !nullable) {
        return `${objectExpr}[${JSON.stringify(key)}] === undefined
        ? dtoDefaultProtocol${pascal(field.type.name)}()
        : ${validate}(${objectExpr}[${JSON.stringify(key)}] as ProtocolJson, ${child})`;
      }
      if (nullable) {
        return `dtoRequired(${objectExpr}, ${JSON.stringify(key)}, ${pathExpr}) === null
        ? null
        : ${validate}(dtoRequired(${objectExpr}, ${JSON.stringify(key)}, ${pathExpr}), ${child})`;
      }
      return `${validate}(dtoRequired(${objectExpr}, ${JSON.stringify(key)}, ${pathExpr}), ${child})`;
    }
    case 'list': {
      const inner = field.type.of;
      if (inner.kind !== 'named') {
        return `dtoList(${base})`;
      }
      const validate = `validateProtocol${pascal(inner.name)}`;
      return `dtoList(${base}).map((item, index) =>
      ${validate}(item, ${pathExpr} + ".${key}[" + index + "]"),
    )`;
    }
    default:
      fail(`unknown type kind ${field.type.kind}`);
  }
}

/** Type default used by an optional (serde `default`) field initializer. */
function tsDefault(field, types) {
  if (field.nullable) return 'null';
  const kind = field.type;
  switch (kind.kind) {
    case 'string':
      return "''";
    case 'i64':
    case 'i32':
      return '0';
    case 'bool':
      return 'false';
    case 'json':
      return 'null';
    case 'list':
      return '[]';
    case 'named': {
      const def = types.find((t) => t.name === kind.name);
      if (!def) fail(`unknown named type ${kind.name}`);
      return `dtoDefaultProtocol${pascal(kind.name)}()`;
    }
    default:
      fail(`unknown default kind ${kind.kind}`);
  }
}

function genTs(schema) {
  const types = schema.types;
  const out = [];
  out.push('// --------------------------------------------------------- DTO shapes');
  out.push('');
  for (const def of types) {
    out.push(`/** ${def.doc} (unknown_fields: ${def.unknown_fields}) */`);
    if (def.shape.kind === 'struct') {
      out.push(`export interface Protocol${def.name} {`);
      for (const f of def.shape.fields) {
        out.push(`  readonly ${f.name}: ${tsType(f.type)}${f.nullable ? ' | null' : ''};`);
      }
      out.push('}');
    } else {
      out.push(`export type Protocol${def.name} =`);
      def.shape.variants.forEach((variant, index) => {
        const fields = variant.fields
          .map((f) => `readonly ${f.name}: ${tsType(f.type)}${f.nullable ? ' | null' : ''}`)
          .join('; ');
        const union = `  | { readonly type: ${JSON.stringify(variant.tag)}; ${fields} }`;
        out.push(`${union}${index === def.shape.variants.length - 1 ? ';' : ''}`);
      });
    }
    out.push('');
  }
  // Default constructors are emitted only for structs referenced by an
  // optional non-null field (serde `default`), so no unused local is
  // generated (tsc runs with noUnusedLocals).
  const neededDefaults = new Set();
  const collect = (def) => {
    if (def.shape.kind !== 'struct') return;
    for (const f of def.shape.fields) {
      if (f.optional && !f.nullable && f.type.kind === 'named') {
        if (!neededDefaults.has(f.type.name)) {
          neededDefaults.add(f.type.name);
          collect(types.find((t) => t.name === f.type.name));
        }
      }
    }
  };
  for (const def of types) collect(def);
  out.push('// ------------------------------------------------------------ defaults');
  out.push('');
  for (const def of types) {
    if (def.shape.kind !== 'struct' || !neededDefaults.has(def.name)) continue;
    out.push(`function dtoDefaultProtocol${def.name}(): Protocol${def.name} {`);
    out.push('  return {');
    for (const f of def.shape.fields) {
      out.push(`    ${f.name}: ${tsDefault(f, types)},`);
    }
    out.push('  };');
    out.push('}');
    out.push('');
  }
  out.push('// ---------------------------------------------------------- validators');
  out.push('');
  for (const def of types) {
    const name = `validateProtocol${def.name}`;
    if (def.shape.kind === 'struct') {
      const allowed = JSON.stringify(def.shape.fields.map((f) => f.name));
      out.push(
        `export function ${name}(value: ProtocolJson, path = ${JSON.stringify(
          `Protocol${def.name}`,
        )}): Protocol${def.name} {`,
      );
      out.push(`  const object = dtoObject(value, path);`);
      out.push(`  const required = ${JSON.stringify(def.shape.fields.filter((f) => !f.optional).map((f) => f.name))};`);
      out.push('  for (const key of required) {');
      out.push('    dtoRequired(object, key, path);');
      out.push('  }');
      if (def.unknown_fields === 'reject') {
        out.push(`  dtoRejectUnknown(object, path, ${allowed});`);
      }
      out.push('  return {');
      for (const f of def.shape.fields) {
        out.push(`    ${f.name}: ${tsExpr(f, 'object', 'path')},`);
      }
      out.push('  };');
      out.push('}');
    } else {
      out.push(
        `export function ${name}(value: ProtocolJson, path = ${JSON.stringify(
          `Protocol${def.name}`,
        )}): Protocol${def.name} {`,
      );
      out.push(`  const object = dtoObject(value, path);`);
      out.push(`  const tag = dtoString(object, ${JSON.stringify(def.shape.tag)}, path);`);
      out.push('  switch (tag) {');
      for (const variant of def.shape.variants) {
        const allowed = JSON.stringify([
          def.shape.tag,
          ...variant.fields.map((f) => f.name),
        ]);
        out.push(`    case ${JSON.stringify(variant.tag)}: {`);
        if (def.unknown_fields === 'reject') {
          out.push(`      dtoRejectUnknown(object, path, ${allowed});`);
        }
        out.push('      return {');
        out.push(`        type: ${JSON.stringify(variant.tag)},`);
        for (const f of variant.fields) {
          out.push(`        ${f.name}: ${tsExpr(f, 'object', 'path')},`);
        }
        out.push('      };');
        out.push('    }');
      }
      out.push('    default:');
      out.push(`      dtoFail(path, "unknown ${def.name} type " + tag);`);
      out.push(`      return null as unknown as Protocol${def.name};`);
      out.push('  }');
      out.push('}');
    }
    out.push('');
  }
  out.push('// ------------------------------------------------- error constants + envelope');
  out.push('');
  out.push('export interface ProtocolErrorEnvelope {');
  out.push('  readonly code: string;');
  out.push('  readonly message: string;');
  out.push('  readonly retryable: boolean;');
  out.push('}');
  out.push('');
  const codes = schema.constants.error_codes;
  out.push('export const PROTOCOL_ERROR_CODES: readonly string[] = [');
  for (const row of codes) out.push(`  ${JSON.stringify(row.code)},`);
  out.push('];');
  out.push('');
  out.push('export const PROTOCOL_ERROR_HTTP_STATUS: { readonly [code: string]: number } = {');
  for (const row of codes) out.push(`  ${JSON.stringify(row.code)}: ${row.http_status},`);
  out.push('};');
  out.push('');
  out.push('export const PROTOCOL_ERROR_RETRYABLE: { readonly [code: string]: boolean } = {');
  for (const row of codes) out.push(`  ${JSON.stringify(row.code)}: ${row.retryable},`);
  out.push('};');
  out.push('');
  out.push(
    '/** Parse the daemon\\\'s typed error envelope `{error:{code,message,retryable}}`.',
  );
  out.push(' * Returns null for any non-conforming body (callers keep their own');
  out.push(' * http_error fallback); a non-boolean retryable reads as false. */');
  out.push(
    'export function parseProtocolErrorEnvelope(value: ProtocolJson): ProtocolErrorEnvelope | null {',
  );
  out.push('  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;');
  out.push('  const error = (value as ProtocolJsonObject)["error"];');
  out.push('  if (typeof error !== "object" || error === null || Array.isArray(error)) return null;');
  out.push('  const object = error as ProtocolJsonObject;');
  out.push('  const code = object["code"];');
  out.push('  const message = object["message"];');
  out.push('  if (typeof code !== "string" || typeof message !== "string") return null;');
  out.push('  return { code, message, retryable: object["retryable"] === true };');
  out.push('}');
  out.push('');
  const body = out.join('\n');
  return [header(schema, '//', ''), '', selectTsHelpers(body, schema), '', body].join('\n');
}

// ---------------------------------------------------------------- Kotlin

function ktType(kind) {
  switch (kind.kind) {
    case 'string':
      return 'String';
    case 'i64':
      return 'Long';
    case 'i32':
      return 'Int';
    case 'bool':
      return 'Boolean';
    case 'json':
      return 'JsonValue';
    case 'named':
      return `Protocol${pascal(kind.name)}`;
    case 'list':
      return `List<${ktType(kind.of)}>`;
    default:
      fail(`unknown type kind ${kind.kind}`);
  }
}

function ktCamel(name) {
  return name.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());
}

function ktExpr(field, view, types) {
  const key = JSON.stringify(field.name);
  switch (field.type.kind) {
    case 'string':
      return field.nullable ? `${view}.optionalField(${key})?.string()` : `${view}.field(${key}).string()`;
    case 'i64':
      return field.nullable ? `${view}.optionalField(${key})?.long()` : `${view}.field(${key}).long()`;
    case 'i32':
      return field.nullable ? `${view}.optionalField(${key})?.int()` : `${view}.field(${key}).int()`;
    case 'bool':
      return `${view}.field(${key}).bool()`;
    case 'json':
      return field.nullable
        ? `${view}.optionalField(${key})?.value`
        : `${view}.field(${key}).value`;
    case 'named': {
      const parse = `parseProtocol${pascal(field.type.name)}`;
      if (field.optional) {
        const def = types.find((t) => t.name === field.type.name);
        if (!def || def.shape.kind !== 'struct') {
          fail(`optional field ${field.name} references a non-struct type`);
        }
        const fallback = `Protocol${pascal(field.type.name)}(${def.shape.fields
          .map((f) => ktDefaultArgs(f, types))
          .join(', ')})`;
        return `${view}.optionalField(${key})?.let { ${parse}(it) } ?: ${fallback}`;
      }
      if (field.nullable) {
        return `${view}.optionalField(${key})?.let { ${parse}(it) }`;
      }
      return `${parse}(${view}.field(${key}))`;
    }
    case 'list': {
      const inner = field.type.of;
      if (inner.kind !== 'named') {
        return `${view}.field(${key}).array().map { it.value }`;
      }
      const parse = `parseProtocol${pascal(inner.name)}`;
      return `${view}.field(${key}).array().map { ${parse}(it) }`;
    }
    default:
      fail(`unknown type kind ${field.type.kind}`);
  }
}

function ktDefaultArgs(field, types) {
  if (field.nullable) return 'null';
  const kind = field.type;
  switch (kind.kind) {
    case 'string':
      return '""';
    case 'i64':
      return '0L';
    case 'i32':
      return '0';
    case 'bool':
      return 'false';
    case 'json':
      return 'JsonValue.Null';
    case 'list':
      return 'emptyList()';
    case 'named': {
      const def = types.find((t) => t.name === kind.name);
      if (!def) fail(`unknown named type ${kind.name}`);
      if (def.shape.kind !== 'struct') fail(`cannot default non-struct ${kind.name}`);
      return `Protocol${pascal(kind.name)}(${def.shape.fields
        .map((f) => ktDefaultArgs(f, types))
        .join(', ')})`;
    }
    default:
      fail(`unknown default kind ${kind.kind}`);
  }
}

function ktStrictCheck(def, allowed, view) {
  if (def.unknown_fields !== 'reject') return [];
  const list = allowed.map((a) => JSON.stringify(a)).join(', ');
  return [
    `    val fields = (${view}.value as? JsonValue.Obj)?.fields ?: emptyMap()`,
    `    for (key in fields.keys) {`,
    `        if (key !in listOf(${list})) {`,
    `            throw NativeProtocolException(${view}.path, "unknown field " + key)`,
    `        }`,
    `    }`,
  ];
}

function genKotlin(schema) {
  const types = schema.types;
  const out = [];
  out.push(header(schema, '//', ''));
  out.push('package dev.faktor.shared');
  out.push('');
  out.push('// The plain DTO/parser portion generated from the canonical schema. The');
  out.push('// JSON model (JsonValue/JsonCodec), NativeProtocolException and the');
  out.push('// handwritten native-wire DTOs/parsers live in NativeProtocol.kt.');
  out.push('');
  out.push('// --------------------------------------------------------- DTO shapes');
  out.push('');
  for (const def of types) {
    out.push(`/** ${def.doc} (unknown_fields: ${def.unknown_fields}) */`);
    if (def.shape.kind === 'struct') {
      out.push(`data class Protocol${def.name}(`);
      def.shape.fields.forEach((f, index) => {
        const comma = index === def.shape.fields.length - 1 ? '' : ',';
        out.push(
          `    val ${ktCamel(f.name)}: ${ktType(f.type)}${f.nullable ? '?' : ''}${comma}`,
        );
      });
      out.push(')');
    } else {
      out.push(`sealed class Protocol${def.name} {`);
      for (const variant of def.shape.variants) {
        out.push(`    data class ${variant.name}(`);
        variant.fields.forEach((f, index) => {
          const comma = index === variant.fields.length - 1 ? '' : ',';
          out.push(
            `        val ${ktCamel(f.name)}: ${ktType(f.type)}${f.nullable ? '?' : ''}${comma}`,
          );
        });
        out.push(`    ) : Protocol${def.name}()`);
      }
      out.push('}');
    }
    out.push('');
  }
  out.push('// -------------------------------------------------------- parse functions');
  out.push('');
  for (const def of types) {
    out.push(`fun parseProtocol${def.name}(v: JsonView): Protocol${def.name} {`);
    if (def.shape.kind === 'struct') {
      const lines = ktStrictCheck(
        def,
        def.shape.fields.map((f) => f.name),
        'v',
      );
      out.push(...lines);
      out.push(`    return Protocol${def.name}(`);
      for (const f of def.shape.fields) {
        out.push(`        ${ktCamel(f.name)} = ${ktExpr(f, 'v', types)},`);
      }
      out.push('    )');
    } else {
      out.push(`    val tag = v.field(${JSON.stringify(def.shape.tag)}).string()`);
      out.push('    return when (tag) {');
      for (const variant of def.shape.variants) {
        out.push(`        ${JSON.stringify(variant.tag)} -> {`);
        const lines = ktStrictCheck(
          def,
          [def.shape.tag, ...variant.fields.map((f) => f.name)],
          'v',
        );
        out.push(...lines);
        out.push(`            Protocol${def.name}.${variant.name}(`);
        for (const f of variant.fields) {
          out.push(`                ${ktCamel(f.name)} = ${ktExpr(f, 'v', types)},`);
        }
        out.push('            )');
        out.push('        }');
      }
      out.push(
        `        else -> throw NativeProtocolException(v.path, "unknown ${def.name} type " + tag)`,
      );
      out.push('    }');
    }
    out.push('}');
    out.push('');
  }
  out.push('// ------------------------------------------------- error constants + envelope');
  out.push('');
  out.push(
    '/** The daemon\'s typed error envelope `{error:{code,message,retryable}}`. */',
  );
  out.push('data class ProtocolErrorEnvelope(');
  out.push('    val code: String,');
  out.push('    val message: String,');
  out.push('    val retryable: Boolean');
  out.push(')');
  out.push('');
  out.push('object ProtocolErrorCodes {');
  const codes = schema.constants.error_codes;
  out.push('    val CODES: List<String> = listOf(');
  for (const row of codes) out.push(`        ${JSON.stringify(row.code)},`);
  out.push('    )');
  out.push('');
  out.push('    val HTTP_STATUS: Map<String, Int> = mapOf(');
  for (const row of codes) out.push(`        ${JSON.stringify(row.code)} to ${row.http_status},`);
  out.push('    )');
  out.push('');
  out.push('    val RETRYABLE: Map<String, Boolean> = mapOf(');
  for (const row of codes)
    out.push(`        ${JSON.stringify(row.code)} to ${row.retryable},`);
  out.push('    )');
  out.push('}');
  out.push('');
  out.push('/** Parse the error envelope; null for any non-conforming body (callers');
  out.push(' * keep their own http_error fallback). A non-boolean retryable reads as');
  out.push(' * false, matching the VS Code client. */');
  out.push('fun parseProtocolErrorEnvelope(value: JsonValue): ProtocolErrorEnvelope? {');
  out.push('    val root = (value as? JsonValue.Obj)?.fields?.get("error") as? JsonValue.Obj');
  out.push('        ?: return null');
  out.push('    val code = (root.fields["code"] as? JsonValue.Str)?.value ?: return null');
  out.push('    val message = (root.fields["message"] as? JsonValue.Str)?.value ?: return null');
  out.push('    val retryable = (root.fields["retryable"] as? JsonValue.Bool)?.value ?: false');
  out.push('    return ProtocolErrorEnvelope(code, message, retryable)');
  out.push('}');
  out.push('');
  return out.join('\n');
}

// ------------------------------------------------------------------- modes

function checkClients(schema) {
  let drifted = false;
  const targets = [
    [TS_TARGET, genTs(schema)],
    [KT_TARGET, genKotlin(schema)],
  ];
  for (const [path, expected] of targets) {
    const full = resolve(ROOT, path);
    if (!existsSync(full)) {
      console.error(`protocol-codegen: missing checked-in generated file ${path}`);
      drifted = true;
      continue;
    }
    drifted = compare(path, expected, readFileSync(full, 'utf8')) || drifted;
  }
  return drifted;
}

function modeCheckFull() {
  readSchema();
  const work = mkdtempSync(join(tmpdir(), 'faktor-protocol-codegen-'));
  try {
    const emitted = join(work, 'schema.json');
    emitArtifactTo(emitted);
    let drifted = compare(ARTIFACT, readFileSync(resolve(ROOT, ARTIFACT), 'utf8'), readFileSync(emitted, 'utf8'));
    const { schema } = readSchema();
    drifted = checkClients(schema) || drifted;
    if (drifted) {
      console.error(
        'protocol-codegen: drift detected; run `node scripts/protocol-codegen.mjs --write` and commit the result',
      );
      process.exit(1);
    }
    console.log(
      `protocol-codegen: OK (artifact + TS/Kotlin clients match ${schema.types.length} types, ${schema.constants.error_codes.length} error codes)`,
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

function modeCheckClients() {
  const { schema } = readSchema();
  if (checkClients(schema)) {
    console.error('protocol-codegen: client drift detected (artifact checked in)');
    process.exit(1);
  }
  console.log(
    `protocol-codegen: OK (clients match ${ARTIFACT}: ${schema.types.length} types, ${schema.constants.error_codes.length} error codes)`,
  );
}

function modeWrite() {
  readSchema();
  emitArtifactTo(resolve(ROOT, ARTIFACT));
  const { schema } = readSchema();
  const outputs = [
    [TS_TARGET, genTs(schema)],
    [KT_TARGET, genKotlin(schema)],
  ];
  for (const [path, text] of outputs) {
    const full = resolve(ROOT, path);
    mkdirSync(dirname(full), { recursive: true });
    writeFileSync(full, text, 'utf8');
    console.log(`protocol-codegen: wrote ${path}`);
  }
  console.log(`protocol-codegen: wrote ${ARTIFACT}`);
}

function modeSelftest() {
  const { schema } = readSchema();
  const ts = genTs(schema);
  const kt = genKotlin(schema);
  const checks = [
    ['TS header', ts.startsWith('// GENERATED FILE')],
    ['TS message validator', ts.includes('export function validateProtocolMessage(')],
    ['TS tagged enum', ts.includes("readonly type: \"tool_result\"")],
    ['TS error constants', ts.includes('export const PROTOCOL_ERROR_CODES')],
    ['Kotlin header', kt.startsWith('// GENERATED FILE')],
    ['Kotlin data class', kt.includes('data class ProtocolMessage(')],
    ['Kotlin sealed enum', kt.includes('sealed class ProtocolPart')],
    ['Kotlin error parser', kt.includes('fun parseProtocolErrorEnvelope(')],
  ];
  let failed = 0;
  for (const [label, ok] of checks) {
    if (!ok) {
      console.error(`protocol-codegen selftest: FAIL ${label}`);
      failed += 1;
    }
  }
  // Drift comparator must catch a one-byte mutation and pass identical text.
  if (compare('selftest', ts, ts)) failed += 1;
  const mutated = ts.replace('validateProtocolMessage', 'validateProtocolMessageX');
  if (!compare('selftest', ts, mutated)) {
    console.error('protocol-codegen selftest: FAIL mutation not detected');
    failed += 1;
  }
  // The schema only needs one type list and one constant table.
  if (schema.types.length < 7 || schema.constants.error_codes.length < 14) {
    console.error('protocol-codegen selftest: FAIL schema surface shrank unexpectedly');
    failed += 1;
  }
  if (failed > 0) process.exit(1);
  console.log('protocol-codegen selftest: PASS');
}

const mode = process.argv[2] || '--check';
if (mode === '--check') modeCheckFull();
else if (mode === '--check-clients') modeCheckClients();
else if (mode === '--write') modeWrite();
else if (mode === '--selftest') modeSelftest();
else {
  console.error(
    'usage: node scripts/protocol-codegen.mjs [--check|--check-clients|--write|--selftest]',
  );
  process.exit(2);
}
